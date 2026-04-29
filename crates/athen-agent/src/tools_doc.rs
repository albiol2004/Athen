//! Generates per-group markdown references of the agent's currently
//! available tools.
//!
//! Layout under the configured directory (typically `~/.athen/tools/`):
//! - `calendar.md`, `contacts.md`, `files.md`, ... — one file per group,
//!   each containing the full schema for every tool in that group.
//!
//! The agent reads only the group it needs via `read`, so loading the
//! schema for one group doesn't pull every other group into context. This
//! replaces both the synthetic `get_tool_details` meta-tool (which models
//! didn't use reliably) and a single monolithic TOOLS.md (which would load
//! every schema on each read).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use athen_core::tool::ToolDefinition;
use serde_json::Value;

use crate::tool_grouping::{group_for, summarize_groups};

/// Render the markdown for a single group.
pub fn generate_group(display_name: &str, tools: &[&ToolDefinition]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {display_name} tools\n\n"));
    out.push_str(
        "Schemas for every tool in this group. Pass arguments as a JSON \
         object matching the schema below.\n\n",
    );
    let mut sorted: Vec<&&ToolDefinition> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in sorted {
        out.push_str(&format!("## `{}`\n\n", tool.name));
        out.push_str(&format!("{}\n\n", tool.description.trim()));
        out.push_str("**Arguments:**\n\n");
        out.push_str("```json\n");
        out.push_str(&pretty_schema(&tool.parameters));
        out.push_str("\n```\n\n");
    }
    out
}

fn pretty_schema(schema: &Value) -> String {
    serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
}

/// Write one markdown file per tool group into `dir`, creating the directory
/// if needed. Returns the list of (group_id, absolute_path) pairs that were
/// written so the system prompt can point the agent at them.
///
/// Existing `*.md` files in `dir` for groups that no longer exist are
/// removed so stale entries don't confuse the agent after MCPs are toggled
/// off.
pub fn write_per_group(
    dir: &Path,
    tools: &[ToolDefinition],
) -> std::io::Result<Vec<(String, PathBuf)>> {
    std::fs::create_dir_all(dir)?;

    let mut by_group: BTreeMap<&str, Vec<&ToolDefinition>> = BTreeMap::new();
    for t in tools {
        by_group.entry(group_for(&t.name)).or_default().push(t);
    }
    let groups = summarize_groups(tools);

    // Track which files we wrote this run so we can clean up stale ones.
    let mut written: Vec<(String, PathBuf)> = Vec::new();
    for g in &groups {
        let Some(group_tools) = by_group.get(g.id.as_str()) else {
            continue;
        };
        let path = dir.join(format!("{}.md", g.id));
        let body = generate_group(&g.display_name, group_tools);
        std::fs::write(&path, body)?;
        written.push((g.id.clone(), path));
    }

    // Remove .md files in the dir that don't correspond to a current group.
    let current_files: std::collections::HashSet<PathBuf> =
        written.iter().map(|(_, p)| p.clone()).collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") && !current_files.contains(&p) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    Ok(written)
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
    fn writes_one_file_per_group() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = vec![
            def("calendar_create", "Create event", json!({"type": "object"})),
            def("calendar_list", "List events", json!({"type": "object"})),
            def(
                "files__write_file",
                "Write file",
                json!({"type": "object", "required": ["path"]}),
            ),
        ];
        let written = write_per_group(tmp.path(), &tools).unwrap();
        let groups: std::collections::HashSet<&str> =
            written.iter().map(|(g, _)| g.as_str()).collect();
        assert!(groups.contains("calendar"));
        assert!(groups.contains("files"));

        let cal = std::fs::read_to_string(tmp.path().join("calendar.md")).unwrap();
        assert!(cal.contains("calendar_create"));
        assert!(cal.contains("calendar_list"));
        // Calendar file should NOT contain files schemas.
        assert!(!cal.contains("files__write_file"));

        let files = std::fs::read_to_string(tmp.path().join("files.md")).unwrap();
        assert!(files.contains("files__write_file"));
        assert!(!files.contains("calendar_create"));
    }

    #[test]
    fn schema_appears_as_json_block() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = vec![def(
            "calendar_create",
            "Create an event",
            json!({
                "type": "object",
                "properties": { "title": {"type": "string"} },
                "required": ["title"]
            }),
        )];
        write_per_group(tmp.path(), &tools).unwrap();
        let cal = std::fs::read_to_string(tmp.path().join("calendar.md")).unwrap();
        assert!(cal.contains("```json"));
        assert!(cal.contains("\"title\""));
        assert!(cal.contains("\"required\""));
    }

    #[test]
    fn creates_directory_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("tools");
        let tools = vec![def("calendar_list", "List events", json!({}))];
        write_per_group(&dir, &tools).unwrap();
        assert!(dir.join("calendar.md").exists());
    }

    #[test]
    fn empty_tool_list_writes_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let written = write_per_group(tmp.path(), &[]).unwrap();
        assert!(written.is_empty());
    }

    #[test]
    fn stale_group_files_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        // First write: calendar + files.
        let initial = vec![
            def("calendar_list", "x", json!({})),
            def("files__read_file", "x", json!({})),
        ];
        write_per_group(tmp.path(), &initial).unwrap();
        assert!(tmp.path().join("files.md").exists());

        // Second write: only calendar (user disabled the Files MCP).
        let after = vec![def("calendar_list", "x", json!({}))];
        write_per_group(tmp.path(), &after).unwrap();
        assert!(tmp.path().join("calendar.md").exists());
        assert!(
            !tmp.path().join("files.md").exists(),
            "stale group file should be removed when its group disappears"
        );
    }
}
