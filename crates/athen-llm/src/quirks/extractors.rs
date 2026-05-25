//! Inline tool-call extractors.
//!
//! Each function takes a response's `content` text, recovers any tool calls
//! the wire format buried inline, and returns `(stripped_content, calls)`
//! where `stripped_content` is the prose with the tool-call markup removed.
//! When no extractor applies or no calls are present, the text is returned
//! unchanged with an empty `Vec`.
//!
//! The extractors are gated by `ToolExtractionStrategy` in
//! `apply_to_response` — they never run for `Structured` (the OpenAI/Anthropic
//! baseline). Output flows through the same `ToolArgRepair` pipeline as
//! native `tool_calls` — extractors must produce the same shape, not bypass
//! repair.

use athen_core::llm::ToolCall;
use serde_json::Value;
use uuid::Uuid;

/// Hermes-JSON variant: `<tool_call>\n{"name": "...", "arguments": {...}}\n</tool_call>`.
/// Used by Qwen3.5/3.6 when the system prompt asks for Hermes-style emission
/// (which Athen's tool index does).
///
/// Qwen-XML variant: `<tool_call><function=NAME><parameter=KEY>VAL</parameter>...</function></tool_call>`.
/// Same wrapper tag, different interior. We try Hermes-JSON first since it's
/// the more common case under llama.cpp's `--jinja` setup with Athen's
/// prompt; fall back to Qwen-XML when the interior isn't valid JSON.
///
/// Returns the content with all `<tool_call>...</tool_call>` blocks removed
/// (whether they parsed or not — leaving them in would confuse the next
/// turn's prompt) and a `Vec<ToolCall>` of everything that did parse.
pub fn extract_qwen_style(content: &str) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut stripped = String::with_capacity(content.len());
    let mut cursor = 0;
    let bytes = content.as_bytes();

    while cursor < bytes.len() {
        let remaining = &content[cursor..];
        let Some(open) = remaining.find("<tool_call>") else {
            stripped.push_str(remaining);
            break;
        };
        // Append prose up to the open tag.
        stripped.push_str(&remaining[..open]);
        let body_start = cursor + open + "<tool_call>".len();
        let after_open = &content[body_start..];
        let Some(close_rel) = after_open.find("</tool_call>") else {
            // Unclosed block — leave as-is so the user/model can see it.
            stripped.push_str(&content[cursor + open..]);
            break;
        };
        let body = &after_open[..close_rel];
        let after_close = body_start + close_rel + "</tool_call>".len();

        if let Some(call) = parse_hermes_json_body(body).or_else(|| parse_qwen_xml_body(body)) {
            calls.push(call);
        }

        cursor = after_close;
    }

    // Trim trailing whitespace introduced by stripping a final block followed
    // by newlines — common Qwen output shape ("<tool_call>...</tool_call>\n").
    let trimmed = stripped.trim_end().to_string();
    (trimmed, calls)
}

fn parse_hermes_json_body(body: &str) -> Option<ToolCall> {
    let trimmed = body.trim();
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    // Hermes spec uses `arguments`; some emitters use `parameters`. Accept either.
    let args = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    Some(ToolCall {
        id: synth_call_id(),
        name,
        arguments: args,
        thought_signature: None,
    })
}

fn parse_qwen_xml_body(body: &str) -> Option<ToolCall> {
    // Qwen wire form (no closing slash on `<function=NAME>`):
    //   <function=NAME>
    //     <parameter=KEY>VALUE</parameter>
    //     <parameter=KEY>VALUE</parameter>
    //   </function>
    let trimmed = body.trim();
    let func_open = trimmed.find("<function=")?;
    let after_eq = &trimmed[func_open + "<function=".len()..];
    let name_end = after_eq.find('>')?;
    let name = after_eq[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let body_after_name = &after_eq[name_end + 1..];
    // Body ends at `</function>` if present, else end of string.
    let interior = match body_after_name.find("</function>") {
        Some(end) => &body_after_name[..end],
        None => body_after_name,
    };

    let mut args_map = serde_json::Map::new();
    let mut scan = interior;
    while let Some(p_open) = scan.find("<parameter=") {
        let after = &scan[p_open + "<parameter=".len()..];
        let Some(key_end) = after.find('>') else {
            break;
        };
        let key = after[..key_end].trim().to_string();
        let val_start = key_end + 1;
        let after_val_start = &after[val_start..];
        let Some(close) = after_val_start.find("</parameter>") else {
            break;
        };
        let raw_val = &after_val_start[..close];
        // Heuristic typing: try JSON parse first (catches numbers, bools,
        // arrays, objects, JSON strings); fall back to plain string.
        let value = serde_json::from_str::<Value>(raw_val.trim())
            .unwrap_or_else(|_| Value::String(raw_val.to_string()));
        if !key.is_empty() {
            args_map.insert(key, value);
        }
        scan = &after_val_start[close + "</parameter>".len()..];
    }

    Some(ToolCall {
        id: synth_call_id(),
        name,
        arguments: Value::Object(args_map),
        thought_signature: None,
    })
}

fn synth_call_id() -> String {
    // The wire didn't carry an `id`. Synthesize one so downstream code that
    // round-trips id back as `tool_call_id` on tool-result messages doesn't
    // collide across calls in the same turn.
    format!("call_{}", Uuid::new_v4().simple())
}

/// MiniMax M2.7 bracket-delimited tool calls:
/// `[TOOL_CALL] {tool => "NAME", args => {--key "value" ...}} [/TOOL_CALL]`.
///
/// Ruby-hash body with CLI-flag-style args (`--key "value"` or `--key value`).
/// Multiple `[TOOL_CALL]...[/TOOL_CALL]` blocks per response are supported.
/// Prose outside the blocks is preserved as stripped content.
pub fn extract_minimax_m27_bracket(content: &str) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut stripped = String::with_capacity(content.len());
    let mut cursor = 0;

    while cursor < content.len() {
        let remaining = &content[cursor..];
        let Some(open) = remaining.find("[TOOL_CALL]") else {
            stripped.push_str(remaining);
            break;
        };
        // Append prose up to the open tag.
        stripped.push_str(&remaining[..open]);
        let body_start = cursor + open + "[TOOL_CALL]".len();
        let after_open = &content[body_start..];
        let Some(close_rel) = after_open.find("[/TOOL_CALL]") else {
            // Unclosed block — leave the rest as-is.
            stripped.push_str(&content[cursor + open..]);
            break;
        };
        let body = &after_open[..close_rel];
        let after_close = body_start + close_rel + "[/TOOL_CALL]".len();

        if let Some(call) = parse_m27_bracket_body(body) {
            calls.push(call);
        }

        cursor = after_close;
    }

    let trimmed = stripped.trim().to_string();
    (trimmed, calls)
}

/// Parse the inner body of a MiniMax M2.7 bracket tool call:
/// `{tool => "NAME", args => {--key "value" --flag bare ...}}`
fn parse_m27_bracket_body(body: &str) -> Option<ToolCall> {
    let trimmed = body.trim();
    // Strip outer braces if present.
    let inner = trimmed.strip_prefix('{')?.strip_suffix('}')?.trim();

    // Extract tool name: `tool => "NAME"`
    let tool_marker = "tool";
    let tool_pos = inner.find(tool_marker)?;
    let after_tool = inner[tool_pos + tool_marker.len()..].trim_start();
    let after_arrow = after_tool.strip_prefix("=>")?;
    let after_arrow = after_arrow.trim_start();
    // Name may be quoted or unquoted.
    let name = if let Some(after_quote) = after_arrow.strip_prefix('"') {
        let end_quote = after_quote.find('"')?;
        after_quote[..end_quote].to_string()
    } else {
        // Bare word up to comma or whitespace.
        let end = after_arrow
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(after_arrow.len());
        after_arrow[..end].to_string()
    };
    if name.is_empty() {
        return None;
    }

    // Extract args block: `args => { ... }`
    let args_marker = "args";
    let args = if let Some(args_pos) = inner.find(args_marker) {
        let after_args = inner[args_pos + args_marker.len()..].trim_start();
        if let Some(after_arrow) = after_args.strip_prefix("=>") {
            let after_arrow = after_arrow.trim_start();
            if let Some(brace_content) = after_arrow.strip_prefix('{') {
                // Find the matching closing brace. The outer `}` is already
                // stripped, so this is the args inner brace. We need to handle
                // possible nested braces in values (unlikely but defensive).
                let close = find_matching_brace(brace_content);
                let args_inner = &brace_content[..close];
                parse_m27_flag_args(args_inner)
            } else {
                serde_json::Map::new()
            }
        } else {
            serde_json::Map::new()
        }
    } else {
        serde_json::Map::new()
    };

    Some(ToolCall {
        id: synth_call_id(),
        name,
        arguments: Value::Object(args),
        thought_signature: None,
    })
}

/// Find the position of the matching `}` for an already-opened `{`.
/// Returns the index of the closing brace, or the end of string if unmatched.
fn find_matching_brace(s: &str) -> usize {
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && in_quote {
            escape = true;
            continue;
        }
        if c == '"' {
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
        }
    }
    s.len()
}

/// Parse CLI-flag-style args: `--key "value" --flag bare --num 42`.
/// Returns a JSON object. Values are always stored as strings — the consumer
/// (tool dispatch) handles type coercion.
fn parse_m27_flag_args(input: &str) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return map;
    }

    let mut cursor = 0;
    let bytes = trimmed.as_bytes();

    while cursor < bytes.len() {
        // Skip whitespace.
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }

        // Look for `--key` prefix.
        if cursor + 1 < bytes.len() && bytes[cursor] == b'-' && bytes[cursor + 1] == b'-' {
            cursor += 2; // skip `--`
                         // Read key: word chars (alphanumeric + hyphen + underscore).
            let key_start = cursor;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric()
                    || bytes[cursor] == b'-'
                    || bytes[cursor] == b'_')
            {
                cursor += 1;
            }
            let key = trimmed[key_start..cursor].to_string();
            if key.is_empty() {
                continue;
            }

            // Skip whitespace between key and value.
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                // Flag with no value — treat as boolean true.
                map.insert(key, Value::String("true".into()));
                break;
            }

            // If next token is another `--`, this flag has no value (boolean).
            if cursor + 1 < bytes.len() && bytes[cursor] == b'-' && bytes[cursor + 1] == b'-' {
                map.insert(key, Value::String("true".into()));
                continue;
            }

            // Read value: quoted string or bare word.
            let value = if bytes[cursor] == b'"' {
                cursor += 1; // skip opening quote
                let val_start = cursor;
                // Read until closing quote, handling escaped quotes.
                let mut val = String::new();
                while cursor < bytes.len() {
                    if bytes[cursor] == b'\\'
                        && cursor + 1 < bytes.len()
                        && bytes[cursor + 1] == b'"'
                    {
                        val.push('"');
                        cursor += 2;
                    } else if bytes[cursor] == b'"' {
                        cursor += 1; // skip closing quote
                        break;
                    } else {
                        val.push(trimmed[cursor..].chars().next().unwrap());
                        cursor += 1;
                    }
                }
                if val.is_empty() {
                    // Fallback if the loop above didn't accumulate (simple case).
                    let end = cursor.saturating_sub(1);
                    trimmed[val_start..end].to_string()
                } else {
                    val
                }
            } else {
                // Bare word: read until whitespace or end.
                let val_start = cursor;
                while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                trimmed[val_start..cursor].to_string()
            };

            map.insert(key, Value::String(value));
        } else {
            // Not a flag — skip to next whitespace (unexpected token).
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
    }

    map
}

/// Strip a single leading `<think>...</think>` block from `content`, plus any
/// trailing whitespace after the closing tag. No-op when the content does not
/// open with a think tag, or when the tag is malformed (we emit as-is rather
/// than try to be clever about partial / nested forms — keeping the user
/// signal beats hiding broken output).
pub fn strip_leading_think_tag(content: &str) -> String {
    let trimmed_start = content.trim_start();
    let leading_ws = &content[..content.len() - trimmed_start.len()];
    if !trimmed_start.starts_with("<think>") {
        return content.to_string();
    }
    let after_open = &trimmed_start["<think>".len()..];
    let Some(close) = after_open.find("</think>") else {
        return content.to_string();
    };
    let tail = &after_open[close + "</think>".len()..];
    let mut out = String::with_capacity(content.len());
    out.push_str(leading_ws);
    out.push_str(tail.trim_start());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_qwen_xml_single_call() {
        let content = "Let me read the file.\n<tool_call><function=read_file><parameter=path>src/main.rs</parameter></function></tool_call>";
        let (stripped, calls) = extract_qwen_style(content);
        assert_eq!(stripped, "Let me read the file.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "src/main.rs");
    }

    #[test]
    fn extract_qwen_xml_multiple_params_with_typing() {
        let content = r#"<tool_call><function=write_file><parameter=path>foo.txt</parameter><parameter=mode>644</parameter><parameter=overwrite>true</parameter></function></tool_call>"#;
        let (_, calls) = extract_qwen_style(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["path"], "foo.txt");
        // Numbers and bools get typed via the JSON-first heuristic.
        assert_eq!(calls[0].arguments["mode"], 644);
        assert_eq!(calls[0].arguments["overwrite"], true);
    }

    #[test]
    fn extract_hermes_json_call() {
        let content = "Sure!\n<tool_call>\n{\"name\": \"list_dir\", \"arguments\": {\"path\": \".\"}}\n</tool_call>\n";
        let (stripped, calls) = extract_qwen_style(content);
        assert_eq!(stripped, "Sure!");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_dir");
        assert_eq!(calls[0].arguments["path"], ".");
    }

    #[test]
    fn extract_hermes_accepts_parameters_alias() {
        let content =
            r#"<tool_call>{"name": "grep", "parameters": {"pattern": "TODO"}}</tool_call>"#;
        let (_, calls) = extract_qwen_style(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["pattern"], "TODO");
    }

    #[test]
    fn extract_multiple_blocks_in_one_response() {
        let content = "First call.\n<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\nNote.\n<tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call>";
        let (stripped, calls) = extract_qwen_style(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
        assert!(stripped.contains("First call."));
        assert!(stripped.contains("Note."));
        assert!(!stripped.contains("<tool_call>"));
    }

    #[test]
    fn unclosed_tool_call_is_preserved_in_content() {
        let content = "Plan: <tool_call>{\"name\":\"x\"";
        let (stripped, calls) = extract_qwen_style(content);
        assert!(calls.is_empty());
        assert!(stripped.contains("<tool_call>"));
    }

    #[test]
    fn no_tool_call_passes_through_unchanged() {
        let content = "Just prose, no tool calls here.";
        let (stripped, calls) = extract_qwen_style(content);
        assert!(calls.is_empty());
        assert_eq!(stripped, content);
    }

    #[test]
    fn malformed_xml_body_is_dropped_but_block_is_stripped() {
        // Stripping is the right call: leaving `<tool_call>` markup in the
        // assistant's content corrupts the next turn's prompt context.
        let content = "<tool_call>not valid anything</tool_call>";
        let (stripped, calls) = extract_qwen_style(content);
        assert!(calls.is_empty());
        assert_eq!(stripped, "");
    }

    #[test]
    fn strip_think_tag_removes_leading_block() {
        let content = "<think>let me think...</think>\nThe answer is 42.";
        assert_eq!(strip_leading_think_tag(content), "The answer is 42.");
    }

    #[test]
    fn strip_think_tag_preserves_leading_whitespace_position() {
        let content = "  <think>x</think>The answer.";
        assert_eq!(strip_leading_think_tag(content), "  The answer.");
    }

    #[test]
    fn strip_think_tag_no_op_when_absent() {
        let content = "No think tag here.";
        assert_eq!(strip_leading_think_tag(content), content);
    }

    #[test]
    fn strip_think_tag_no_op_when_unclosed() {
        // Don't try to be clever about malformed tags — emit as-is.
        let content = "<think>partial...";
        assert_eq!(strip_leading_think_tag(content), content);
    }

    #[test]
    fn strip_think_tag_only_strips_leading_block() {
        // A think tag in the middle of prose stays put. Only a single
        // leading block is the documented contract.
        let content = "Prefix <think>middle</think> suffix";
        assert_eq!(strip_leading_think_tag(content), content);
    }

    // -----------------------------------------------------------------------
    // MiniMax M2.7 bracket extractor
    // -----------------------------------------------------------------------

    #[test]
    fn extract_m27_basic_observed_sample() {
        let content = r#"Continue [TOOL_CALL] {tool => "shell_spawn", args => {
--cmd "python3 dashboard_server.py"
--directory "/home/alex/.athen/workspace"
}} [/TOOL_CALL]"#;
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert_eq!(stripped, "Continue");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_spawn");
        assert_eq!(calls[0].arguments["cmd"], "python3 dashboard_server.py");
        assert_eq!(
            calls[0].arguments["directory"],
            "/home/alex/.athen/workspace"
        );
    }

    #[test]
    fn extract_m27_multiple_tool_calls() {
        let content = r#"First step [TOOL_CALL] {tool => "read_file", args => {
--path "src/main.rs"
}} [/TOOL_CALL]
Then [TOOL_CALL] {tool => "write_file", args => {
--path "out.txt"
--content "hello"
}} [/TOOL_CALL] done."#;
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "src/main.rs");
        assert_eq!(calls[1].name, "write_file");
        assert_eq!(calls[1].arguments["path"], "out.txt");
        assert_eq!(calls[1].arguments["content"], "hello");
        assert!(stripped.contains("First step"));
        assert!(stripped.contains("Then"));
        assert!(stripped.contains("done."));
        assert!(!stripped.contains("[TOOL_CALL]"));
    }

    #[test]
    fn extract_m27_bare_unquoted_values() {
        let content = r#"[TOOL_CALL] {tool => "shell_execute", args => {
--cmd ls
--timeout 30
}} [/TOOL_CALL]"#;
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_execute");
        // Bare values are stored as strings.
        assert_eq!(calls[0].arguments["cmd"], "ls");
        assert_eq!(calls[0].arguments["timeout"], "30");
        assert!(stripped.is_empty());
    }

    #[test]
    fn extract_m27_no_tool_call() {
        let content = "Just a normal response with no tool calls.";
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert!(calls.is_empty());
        assert_eq!(stripped, content);
    }

    #[test]
    fn extract_m27_malformed_missing_closing_tag() {
        // Missing [/TOOL_CALL] — unclosed block preserved in output.
        let content = r#"Start [TOOL_CALL] {tool => "shell", args => {--cmd "ls"}}"#;
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert!(calls.is_empty());
        assert!(stripped.contains("[TOOL_CALL]"));
    }

    #[test]
    fn extract_m27_empty_args() {
        let content = r#"[TOOL_CALL] {tool => "list_tools", args => {}} [/TOOL_CALL]"#;
        let (stripped, calls) = extract_minimax_m27_bracket(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_tools");
        assert!(calls[0].arguments.as_object().unwrap().is_empty());
        assert!(stripped.is_empty());
    }

    #[test]
    fn extract_m27_synthetic_ids_are_unique() {
        let content = r#"[TOOL_CALL] {tool => "a", args => {}} [/TOOL_CALL] [TOOL_CALL] {tool => "b", args => {}} [/TOOL_CALL]"#;
        let (_, calls) = extract_minimax_m27_bracket(content);
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].id, calls[1].id);
        assert!(calls[0].id.starts_with("call_"));
    }
}
