//! Tool-argument repair flags.
//!
//! Each flag handles a specific wire-level corruption seen in the field.
//! Repairs operate on a `serde_json::Value` (already parsed by the time we
//! get here — provider parsing tolerantly survives most of the originals
//! via the existing per-tool `do_*` repair). This second layer is
//! defense-in-depth at the wire boundary.
//!
//! Adding a new repair: add a flag to `ToolArgRepair` in `mod.rs`, add a
//! match arm here, add a regression test.

use serde_json::Value;

use super::ToolArgRepair;

/// Apply every enabled repair flag to a tool call's argument value, in
/// declaration order. Repairs that don't apply (e.g. wrong shape) are no-ops.
pub fn apply(flags: &ToolArgRepair, value: &mut Value) {
    if flags.control_chars_to_unicode_escape {
        escape_control_chars(value);
    }
    if flags.unescape_double_encoded_json_arrays {
        unescape_double_encoded_arrays(value);
    }
}

/// Replace raw control chars (`0x00..=0x1F`, excluding the four whitespace
/// chars JSON allows: tab, LF, CR, FF) inside string fields. By the time the
/// args are a `Value`, serde_json has already accepted the input — but
/// downstream tool handlers re-serialize the args and pass them through
/// shells / files where embedded control chars would otherwise corrupt
/// command lines or filesystem paths.
fn escape_control_chars(value: &mut Value) {
    match value {
        Value::String(s) if s.chars().any(is_problem_control) => {
            *s = s
                .chars()
                .map(|c| if is_problem_control(c) { ' ' } else { c })
                .collect();
        }
        Value::Array(items) => {
            for v in items {
                escape_control_chars(v);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                escape_control_chars(v);
            }
        }
        _ => {}
    }
}

fn is_problem_control(c: char) -> bool {
    let cp = c as u32;
    cp <= 0x1F && cp != 0x09 && cp != 0x0A && cp != 0x0D && cp != 0x0C
}

/// Gemma 4 via Ollama returns array-typed parameters as escaped JSON
/// *strings*: `{"files": "[\"a.txt\",\"b.txt\"]"}` instead of
/// `{"files": ["a.txt","b.txt"]}`. When a string field looks like a JSON
/// array literal, parse and replace.
fn unescape_double_encoded_arrays(value: &mut Value) {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    if matches!(parsed, Value::Array(_)) {
                        *value = parsed;
                    }
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                unescape_double_encoded_arrays(v);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                unescape_double_encoded_arrays(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn control_chars_replaced_with_space() {
        let mut v = json!({"path": "src/foo\u{0001}.rs"});
        apply(
            &ToolArgRepair {
                control_chars_to_unicode_escape: true,
                ..ToolArgRepair::empty()
            },
            &mut v,
        );
        assert_eq!(v["path"], "src/foo .rs");
    }

    #[test]
    fn control_chars_repair_preserves_allowed_whitespace() {
        let mut v = json!({"text": "line1\nline2\tcol2\rdone"});
        apply(
            &ToolArgRepair {
                control_chars_to_unicode_escape: true,
                ..ToolArgRepair::empty()
            },
            &mut v,
        );
        assert_eq!(v["text"], "line1\nline2\tcol2\rdone");
    }

    #[test]
    fn control_chars_repair_descends_into_arrays_and_objects() {
        let mut v = json!({
            "outer": {
                "list": ["clean", "dirty\u{0007}val"]
            }
        });
        apply(
            &ToolArgRepair {
                control_chars_to_unicode_escape: true,
                ..ToolArgRepair::empty()
            },
            &mut v,
        );
        assert_eq!(v["outer"]["list"][1], "dirty val");
    }

    #[test]
    fn double_encoded_arrays_get_unwrapped() {
        let mut v = json!({"files": "[\"a.txt\",\"b.txt\"]"});
        apply(
            &ToolArgRepair {
                unescape_double_encoded_json_arrays: true,
                ..ToolArgRepair::empty()
            },
            &mut v,
        );
        assert_eq!(v["files"], json!(["a.txt", "b.txt"]));
    }

    #[test]
    fn double_encoded_repair_leaves_real_strings_alone() {
        let mut v = json!({"path": "src/foo.rs"});
        apply(
            &ToolArgRepair {
                unescape_double_encoded_json_arrays: true,
                ..ToolArgRepair::empty()
            },
            &mut v,
        );
        assert_eq!(v["path"], "src/foo.rs");
    }

    #[test]
    fn no_flags_means_no_changes() {
        let original = json!({"path": "x", "ctl": "a\u{0001}b"});
        let mut v = original.clone();
        apply(&ToolArgRepair::empty(), &mut v);
        assert_eq!(v, original);
    }
}
