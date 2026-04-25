//! Generates a markdown reference of the agent's currently available tools.
//!
//! The doc is written to disk (typically `~/.athen/TOOLS.md`) so the agent
//! can read it on demand with the same `read_file` tool it uses for any other
//! file. This replaces the synthetic `get_tool_details` meta-tool: instead
//! of teaching the model a new discovery interface it doesn't reliably use,
//! we lean on a capability it already has.

use std::collections::BTreeMap;

use athen_core::tool::ToolDefinition;
use serde_json::Value;

use crate::tool_grouping::{group_for, summarize_groups};

/// Build the full markdown reference for the given tool list.
///
/// Output structure:
/// 1. Top-level header + brief usage note.
/// 2. One section per tool group, with each tool's name, description, and
///    JSON Schema rendered as a fenced code block.
pub fn generate(tools: &[ToolDefinition]) -> String {
    let mut out = String::new();
    out.push_str("# Athen Tool Reference\n\n");
    out.push_str(
        "This file lists every tool currently available to the agent. \
         Each entry includes the name, a short description, and the JSON \
         schema for its arguments. Read the section for any tool whose \
         schema you don't already know before calling it.\n\n",
    );

    // Quick index — mirrors the system-prompt summary so the agent can
    // jump to the right section quickly.
    out.push_str("## Index\n\n");
    let groups = summarize_groups(tools);
    for g in &groups {
        out.push_str(&format!(
            "- **{}** ({}): {}\n",
            g.display_name,
            g.tool_count(),
            g.one_liner,
        ));
    }
    out.push('\n');

    // Group → tools, ordered the same way summarize_groups returned them.
    let mut by_group: BTreeMap<&str, Vec<&ToolDefinition>> = BTreeMap::new();
    for t in tools {
        by_group.entry(group_for(&t.name)).or_default().push(t);
    }

    for g in &groups {
        out.push_str(&format!("## {}\n\n", g.display_name));
        let Some(group_tools) = by_group.get(g.id.as_str()) else { continue };
        let mut sorted = group_tools.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for tool in sorted {
            out.push_str(&format!("### `{}`\n\n", tool.name));
            out.push_str(&format!("{}\n\n", tool.description.trim()));
            out.push_str("**Arguments:**\n\n");
            out.push_str("```json\n");
            out.push_str(&pretty_schema(&tool.parameters));
            out.push_str("\n```\n\n");
        }
    }

    out
}

fn pretty_schema(schema: &Value) -> String {
    serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
}

/// Write `generate(tools)` to `path`, creating parent directories. Returns
/// any I/O error so callers can decide whether to log or surface it.
pub fn write_to(path: &std::path::Path, tools: &[ToolDefinition]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, generate(tools))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::risk::BaseImpact;
    use athen_core::tool::{ToolBackend, ToolDefinition};
    use serde_json::json;

    fn def(name: &str, desc: &str, params: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params,
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: BaseImpact::Read,
        }
    }

    #[test]
    fn includes_every_tool() {
        let tools = vec![
            def("calendar_create", "Create an event", json!({"type": "object"})),
            def("calendar_list", "List events", json!({"type": "object"})),
            def(
                "files__write_file",
                "Write to a file",
                json!({"type": "object", "required": ["path"]}),
            ),
        ];
        let md = generate(&tools);
        assert!(md.contains("`calendar_create`"));
        assert!(md.contains("`calendar_list`"));
        assert!(md.contains("`files__write_file`"));
    }

    #[test]
    fn groups_tools_by_prefix() {
        let tools = vec![
            def("calendar_create", "x", json!({})),
            def("calendar_list", "x", json!({})),
            def("contacts_list", "x", json!({})),
        ];
        let md = generate(&tools);
        // Each group's heading shows up exactly once.
        assert_eq!(md.matches("## Calendar\n").count(), 1);
        assert_eq!(md.matches("## Contacts\n").count(), 1);
    }

    #[test]
    fn renders_schema_as_json_block() {
        let tools = vec![def(
            "calendar_create",
            "Create an event",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"}
                },
                "required": ["title"]
            }),
        )];
        let md = generate(&tools);
        assert!(md.contains("```json"));
        assert!(md.contains("\"title\""));
        assert!(md.contains("\"required\""));
    }

    #[test]
    fn write_to_creates_file_and_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("TOOLS.md");
        let tools = vec![def("calendar_list", "List events", json!({}))];
        write_to(&path, &tools).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("calendar_list"));
    }

    #[test]
    fn empty_tools_still_writes_header() {
        let md = generate(&[]);
        assert!(md.starts_with("# Athen Tool Reference"));
        assert!(md.contains("## Index"));
    }
}
