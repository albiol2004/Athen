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
}
