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
    // The built-in file primitives (`read`, `edit`, `write`, `grep`,
    // `list_directory`) don't share a `<group>_<verb>` prefix the way
    // `calendar_*` / `shell_*` do, but they belong in one group from the
    // user's and the model's point of view.
    if matches!(name, "read" | "edit" | "write" | "grep" | "list_directory") {
        return "files";
    }
    // Built-in tools follow a "<group>_<verb>" convention. Single-word tools
    // (e.g. "shell_execute" → "shell") fall back to the part before the first
    // underscore.
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
        "setup" => "Setup".to_string(),
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
        "shell" => "execute shell commands".to_string(),
        "files" => "read, edit, write, search and list files".to_string(),
        "web" => "search the web and fetch URLs as clean markdown".to_string(),
        "setup" => {
            "configure Athen's integrations (email, calendar, Telegram, identity, web search)"
                .to_string()
        }
        // Fallback: show the bare tool names so the agent can recognise them.
        _ => format!("tools: {}", names.join(", ")),
    }
}

/// True if the tool should always be revealed (full schema in every request).
/// Always-revealed tools are the universally-used core: the write side of
/// memory (only used on explicit user request) plus the shell + file
/// primitives (the bread-and-butter coding tools). Without their schemas,
/// small models fall back to whatever IS schema-visible and loop.
/// `email_send` is included because it's a user-facing action tool gated
/// behind explicit approval — keeping its schema inline prevents the
/// agent from grepping config files or reaching for `shell_execute` /
/// smtplib when it could just call the tool. Domain-specific tools
/// (calendar, contacts, MCP) stay tier-2.
///
/// `memory_recall` is DELIBERATELY tier-2 (revealed on first call) — the
/// host already injects relevant memories into the leading system message
/// via the `BACKGROUND RECALL` block when the user message is substantive,
/// so the agent rarely needs to call recall directly. Surfacing its full
/// schema in every prompt biased small models to over-recall on
/// short / pronoun-y messages and act on stale entries as if they were
/// instructions.
pub fn is_always_revealed(name: &str) -> bool {
    is_always_revealed_for_profile(name, &[])
}

/// Per-profile variant. When `primary_groups` is non-empty, *only* tools
/// whose group is in that list (plus a small universal core that every
/// profile reaches for) get full schemas in the static prefix. Tools
/// outside primary_groups remain fully callable — the executor auto-
/// reveals their schema on first invocation, see
/// `executor::DefaultExecutor::execute` reveal-on-call path.
///
/// When `primary_groups` is empty, falls back to the global default —
/// the historic always-revealed list, preserving today's behavior for
/// the `default` / `assistant` / `devops` / `technical_support` profiles.
pub fn is_always_revealed_for_profile(name: &str, primary_groups: &[String]) -> bool {
    // Universal core: memory_store is referenced by every "should I remember
    // this" reflex even on profiles that aren't memory-heavy. email_send and
    // send_telegram are kept always-revealed because they're the inbound-
    // reply channels for autonomous dispatch (sense events) — a profile
    // that's never expected to reply via email STILL receives email sense
    // events and must be able to draft one back. If a profile genuinely
    // doesn't want them prominent it can edit its `primary_groups`.
    if matches!(name, "memory_store" | "email_send" | "send_telegram") {
        return true;
    }
    if primary_groups.is_empty() {
        return matches!(
            name,
            "shell_execute"
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
        );
    }
    let group = group_for(name);
    primary_groups.iter().any(|g| g == group)
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
        assert_eq!(group_for("slack__post_message"), "slack");
        assert_eq!(group_for("calendar__list_events"), "calendar");
    }

    #[test]
    fn group_for_builtin_file_primitives_collapses_into_files() {
        // The five canonical file built-ins don't share a `<group>_<verb>`
        // prefix, but they belong in one group for the system prompt.
        assert_eq!(group_for("read"), "files");
        assert_eq!(group_for("edit"), "files");
        assert_eq!(group_for("write"), "files");
        assert_eq!(group_for("grep"), "files");
        assert_eq!(group_for("list_directory"), "files");
    }

    #[test]
    fn profile_aware_reveal_respects_primary_groups() {
        use super::is_always_revealed_for_profile;
        let lawyerish = vec!["web".to_string(), "files".to_string(), "memory".to_string()];
        // Tools in primary_groups are revealed.
        assert!(is_always_revealed_for_profile("web_search", &lawyerish));
        assert!(is_always_revealed_for_profile("web_fetch", &lawyerish));
        assert!(is_always_revealed_for_profile("read", &lawyerish));
        assert!(is_always_revealed_for_profile("memory_store", &lawyerish));
        // Tools outside primary_groups stay tier-2 — agents can still
        // reach them via auto-reveal-on-call.
        assert!(!is_always_revealed_for_profile("shell_execute", &lawyerish));
        assert!(!is_always_revealed_for_profile(
            "calendar_create",
            &lawyerish
        ));
        // Universal core: memory_store / email_send / send_telegram are
        // always tier-1 regardless of the profile's primary_groups.
        let narrow = vec!["web".to_string()];
        assert!(is_always_revealed_for_profile("memory_store", &narrow));
        assert!(is_always_revealed_for_profile("email_send", &narrow));
        assert!(is_always_revealed_for_profile("send_telegram", &narrow));
        // Empty primary_groups falls back to the global default.
        assert!(is_always_revealed_for_profile("shell_execute", &[]));
        assert!(is_always_revealed_for_profile("web_search", &[]));
        assert!(!is_always_revealed_for_profile("calendar_create", &[]));
    }

    #[test]
    fn always_revealed_covers_core_tools() {
        assert!(is_always_revealed("memory_store"));
        // memory_recall is DELIBERATELY tier-2: the host already injects
        // relevant memories into the leading system message via the
        // BACKGROUND RECALL block when the user message is substantive.
        assert!(!is_always_revealed("memory_recall"));
        assert!(is_always_revealed("shell_execute"));
        assert!(is_always_revealed("shell_spawn"));
        assert!(is_always_revealed("read"));
        assert!(is_always_revealed("edit"));
        assert!(is_always_revealed("write"));
        assert!(is_always_revealed("grep"));
        assert!(is_always_revealed("email_send"));
        assert!(!is_always_revealed("calendar_create"));
    }

    #[test]
    fn summarize_groups_collapses_by_prefix() {
        let tools = vec![
            def("memory_store"),
            def("memory_recall"),
            def("calendar_create"),
            def("calendar_list"),
            def("read"),
            def("write"),
            def("list_directory"),
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
        assert!(files.tool_names.contains(&"read".to_string()));
        assert!(files.tool_names.contains(&"write".to_string()));
    }
}
