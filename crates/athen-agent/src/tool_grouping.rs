//! Helpers for the two-tier tool surfacing system.
//!
//! Goal: keep the agent's prompt small without hiding capabilities. Tools are
//! grouped by prefix; only a one-line summary per group is always in the
//! system prompt. Memory tools have their full schemas inlined every request
//! ("always-revealed"). Other tools are listed by name only; the agent calls
//! them directly via tolerant dispatch (the executor reveals their schema on
//! first call) or reads `~/.athen/TOOLS.md` for full details.

use std::collections::BTreeMap;

use athen_core::tool::ToolDefinition;

/// Return the canonical group id for a tool name. The id is the prefix the
/// system prompt groups by — "memory", "calendar", "files", and so on.
pub fn group_for(name: &str) -> &str {
    // MCP tools are namespaced as "<mcp_id>__<tool>"; the mcp_id is the group.
    if let Some((prefix, _)) = name.split_once("__") {
        return prefix;
    }
    // Built-in tools follow a "<group>_<verb>" convention. Single-word tools
    // (e.g. "shell_execute" → "shell", "read_file" → "read") fall back to
    // the part before the first underscore.
    if let Some((prefix, _)) = name.split_once('_') {
        return prefix;
    }
    name
}

/// One group's worth of summary information for the system prompt.
#[derive(Debug, Clone)]
pub struct ToolGroupSummary {
    pub id: String,
    pub display_name: String,
    pub one_liner: String,
    pub tool_names: Vec<String>,
}

impl ToolGroupSummary {
    pub fn tool_count(&self) -> usize {
        self.tool_names.len()
    }
}

/// Build a list of group summaries, sorted by id for stable prompts.
pub fn summarize_groups(tools: &[ToolDefinition]) -> Vec<ToolGroupSummary> {
    let mut by_group: BTreeMap<String, Vec<&ToolDefinition>> = BTreeMap::new();
    for t in tools {
        by_group
            .entry(group_for(&t.name).to_string())
            .or_default()
            .push(t);
    }
    by_group
        .into_iter()
        .map(|(id, ts)| {
            let mut tool_names: Vec<String> = ts.iter().map(|t| t.name.clone()).collect();
            tool_names.sort();
            ToolGroupSummary {
                display_name: pretty_group_name(&id),
                one_liner: group_one_liner(&id, &ts),
                tool_names,
                id,
            }
        })
        .collect()
}

fn pretty_group_name(id: &str) -> String {
    match id {
        "memory" => "Memory".to_string(),
        "calendar" => "Calendar".to_string(),
        "contacts" => "Contacts".to_string(),
        "shell" => "Shell".to_string(),
        "files" => "Files".to_string(),
        "web" => "Web".to_string(),
        // Capitalize first letter for unknown groups (MCPs etc.)
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        }
    }
}

fn group_one_liner(id: &str, tools: &[&ToolDefinition]) -> String {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    match id {
        "memory" => "persistent memory across conversations".to_string(),
        "calendar" => "create, list, update, delete calendar events".to_string(),
        "contacts" => "manage contacts and their identifiers".to_string(),
        "shell" => "execute shell commands and basic file ops".to_string(),
        "files" => "read, write, list and organize files in a sandboxed folder".to_string(),
        "web" => "search the web and fetch URLs as clean markdown".to_string(),
        // Fallback: show the bare tool names so the agent can recognise them.
        _ => format!("tools: {}", names.join(", ")),
    }
}

/// True if the tool should always be revealed (full schema in every request).
/// Always-revealed tools are the universally-used core: memory (referenced on
/// every turn) plus the shell + file primitives (the bread-and-butter coding
/// tools). Without their schemas, small models fall back to whatever IS
/// schema-visible and loop. Domain-specific tools (calendar, contacts, MCP)
/// stay tier-2 to keep the prompt small.
pub fn is_always_revealed(name: &str) -> bool {
    matches!(
        name,
        "memory_store"
            | "memory_recall"
            | "shell_execute"
            | "shell_spawn"
            | "shell_kill"
            | "shell_logs"
            | "read"
            | "edit"
            | "write"
            | "grep"
            | "list_directory"
            | "web_search"
            | "web_fetch"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::risk::BaseImpact;
    use athen_core::tool::ToolBackend;
    use serde_json::json;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("desc for {name}"),
            parameters: json!({}),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: BaseImpact::Read,
        }
    }

    #[test]
    fn group_for_builtin() {
        assert_eq!(group_for("memory_store"), "memory");
        assert_eq!(group_for("calendar_create"), "calendar");
        assert_eq!(group_for("contacts_search"), "contacts");
        assert_eq!(group_for("shell_execute"), "shell");
    }

    #[test]
    fn group_for_mcp_uses_double_underscore() {
        assert_eq!(group_for("files__read_file"), "files");
        assert_eq!(group_for("calendar__list_events"), "calendar");
    }

    #[test]
    fn always_revealed_covers_core_tools() {
        assert!(is_always_revealed("memory_store"));
        assert!(is_always_revealed("memory_recall"));
        assert!(is_always_revealed("shell_execute"));
        assert!(is_always_revealed("shell_spawn"));
        assert!(is_always_revealed("read"));
        assert!(is_always_revealed("edit"));
        assert!(is_always_revealed("write"));
        assert!(is_always_revealed("grep"));
        assert!(!is_always_revealed("calendar_create"));
        assert!(!is_always_revealed("files__write_file"));
    }

    #[test]
    fn summarize_groups_collapses_by_prefix() {
        let tools = vec![
            def("memory_store"),
            def("memory_recall"),
            def("calendar_create"),
            def("calendar_list"),
            def("files__read_file"),
            def("files__write_file"),
            def("files__list_dir"),
        ];
        let summary = summarize_groups(&tools);
        let by_id: std::collections::HashMap<_, _> = summary
            .iter()
            .map(|g| (g.id.as_str(), g.tool_count()))
            .collect();
        assert_eq!(by_id.get("memory"), Some(&2));
        assert_eq!(by_id.get("calendar"), Some(&2));
        assert_eq!(by_id.get("files"), Some(&3));
        let files = summary.iter().find(|g| g.id == "files").unwrap();
        assert!(files.tool_names.contains(&"files__read_file".to_string()));
        assert!(files.tool_names.contains(&"files__write_file".to_string()));
    }
}
