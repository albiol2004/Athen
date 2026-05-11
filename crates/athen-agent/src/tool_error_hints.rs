//! Per-tool error-time reminder snippets.
//!
//! Cursor-style "Error + policy" pairing: when a tool fails, the result
//! the LLM sees is the raw error PLUS a one-line `<system-reminder>` of
//! the most-common-misuse policy for that tool. The model sees the
//! mistake and the rule in the same context window — episodic anchoring
//! that's much stronger than a one-time mention in the system prompt.
//!
//! Hints are intentionally short (≤300 chars each) and pattern-agnostic:
//! they remind the agent of the invariant (e.g. `edit` requires a prior
//! `read`), not the specific failure (the error string covers that).
//!
//! Skipped entirely when the tool itself already wrote a policy message
//! (`loop_guard`, `duplicate_in_batch`) — those have purposeful longer
//! prose and re-anchoring on top would just dilute the signal.

/// Per-tool hint, or `None` for tools without a known common-misuse
/// pattern (no hint → just return the raw error to the model).
pub fn hint_for(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        // ── File primitives ──────────────────────────────────────────
        "edit" => Some(
            "edit requires a prior read of the same file in this session. \
             If you saw 'must read first' or a 'file changed' error, call \
             read on the exact path before retrying. Use workspace-relative \
             paths — Athen resolves them against the workspace dir.",
        ),
        "write" => Some(
            "write replaces the entire file. Existing files require a prior \
             read this session (new files do not). Use workspace-relative \
             paths. For partial edits prefer `edit` over rewriting the whole \
             file.",
        ),
        "read" => Some(
            "read uses workspace-relative paths by default. If a path is \
             missing, call list_directory first to verify the directory \
             contents. Absolute paths outside the workspace may require an \
             approval grant on first touch.",
        ),

        // ── Shell ────────────────────────────────────────────────────
        "shell_execute" => Some(
            "Pick syntax that matches the SHELL ENVIRONMENT block in your \
             system prompt (nushell, sh, or cmd) — bash-only idioms (`&&`, \
             `>file 2>&1`, `nohup CMD &`, `export VAR=`) silently fail under \
             nushell and cmd. For long-running processes use shell_spawn. \
             Do NOT use curl/wget/lynx for web content — use web_fetch or \
             web_search instead.",
        ),
        "shell_spawn" => Some(
            "shell_spawn detaches; use it for servers, watchers, or anything \
             that should outlive the agent turn. Save the returned pid — \
             you need it for shell_kill and shell_logs.",
        ),

        // ── HTTP / cloud APIs ────────────────────────────────────────
        "http_request" => Some(
            "endpoint_id is a UUID — look it up in the REGISTERED CLOUD APIs \
             section of your system prompt (do not invent one). Credentials \
             are pre-loaded in the vault: never hand-roll Authorization \
             headers. Body must be a valid JSON string for application/json \
             endpoints.",
        ),

        // ── Memory ───────────────────────────────────────────────────
        "memory_store" => Some(
            "Search first with memory_recall to avoid duplicates — the \
             store de-dupes silently and a near-match returns no insert. \
             Bodies should be specific facts (\"Alex prefers DeepSeek for \
             coding tasks\"), not generic notes (\"the user likes things\"). \
             Tag with `applies_to` if it should only surface for some profiles.",
        ),
        "memory_recall" => Some(
            "Use specific queries. The cosine threshold is 0.6 — overly \
             generic queries (\"user preferences\") return nothing. If you \
             expect an entry and got empty results, try a more specific \
             phrasing.",
        ),

        // ── Calendar ─────────────────────────────────────────────────
        "calendar_create" | "calendar_update" => Some(
            "Datetime fields are RFC3339 with the LOCAL timezone offset \
             (e.g. `2026-05-11T15:00:00+02:00`), not UTC `Z`. Use the offset \
             shown in your system prompt. Required: title + start. End is \
             optional but recommended.",
        ),
        "calendar_delete" | "calendar_list" => Some(
            "Need a valid event_id from a prior calendar_list. If the id \
             came from a memory or earlier turn, verify with calendar_list \
             first — events can be moved or deleted out-of-band.",
        ),

        // ── Outbound messaging ───────────────────────────────────────
        "send_telegram" => Some(
            "chat_id is required. Owner chat (your user) auto-approves and \
             is the default when chat_id is omitted; non-owner sends route \
             through the approval router. Text OR attachments must be \
             present — empty messages are rejected. Captions cap at 1024 \
             chars; longer text is split into multiple messages.",
        ),
        "email_send" => Some(
            "to / subject / body are required. body is HTML — escape any \
             user-provided text before embedding. Outbound is gated by the \
             approval router for non-owner recipients; owner sends \
             auto-approve.",
        ),

        // ── Web ──────────────────────────────────────────────────────
        "web_search" => Some(
            "Use specific multi-word queries. Single-word or overly broad \
             queries return shallow results. Provider chain runs Brave → \
             Tavily → DDG-floor (configured in Settings).",
        ),
        "web_fetch" => Some(
            "Only http(s) URLs — file://, ftp://, mailto:, javascript: are \
             rejected. If the page looks empty (<150 chars) the fallback \
             chain (Jina → Wayback) auto-runs; the source field tells you \
             which tier answered.",
        ),

        // ── Toolbox / packages ───────────────────────────────────────
        "install_package" => Some(
            "install_package is approval-gated — include a clear reason \
             string. Check list_installed_packages first; reinstalling \
             what's already there wastes a prompt. Specify runtime (python \
             or node) and version_spec.",
        ),

        // ── Contacts ─────────────────────────────────────────────────
        "contacts_search" | "contacts_get" | "contacts_create" | "contacts_update"
        | "contacts_delete" => Some(
            "Search by name OR identifier (email/phone/telegram_id) — not \
             both. Contacts have a TrustLevel that gates risk multipliers; \
             updating it should be deliberate, not reflexive.",
        ),

        // ── Identity ─────────────────────────────────────────────────
        "identity_add" => Some(
            "Only persist genuinely NEW facts the user shared in this turn. \
             The identity store already loaded its contents into your system \
             prompt — duplicating those wastes tokens forever. Set \
             `applies_to` if the fact is profile-specific.",
        ),

        // ── Wake-ups ─────────────────────────────────────────────────
        "create_wakeup" => Some(
            "Wake-ups are scheduled future tasks, not reminders to display. \
             Specify a clear instruction the agent can act on at fire time. \
             AutonomyBand + tool/contact allowlists are pre-approved at \
             creation; risk is still checked per-action at fire time.",
        ),

        _ => None,
    }
}

/// Append a `<system-reminder>` to an error body when a hint is known for
/// the tool, else return the body unchanged. The reminder lives in the
/// tool-result content the LLM sees, so the model gets the error and the
/// policy in the same attention window — episodic anchoring.
///
/// `tool_error` is the value of `ToolResult.error` (when the result was
/// `Ok` but unsuccessful) so we can skip hints when the tool itself
/// already wrote a purposeful policy message (`loop_guard`,
/// `duplicate_in_batch`).
pub fn maybe_append_hint(body: &str, tool_name: &str, tool_error: Option<&str>) -> String {
    // The executor's own short-circuit error messages already carry a
    // strong policy nudge — stacking another reminder on top dilutes
    // the signal and inflates the budget.
    if matches!(tool_error, Some("loop_guard") | Some("duplicate_in_batch")) {
        return body.to_string();
    }
    match hint_for(tool_name) {
        Some(hint) => format!("{body}\n<system-reminder>\n{hint}\n</system-reminder>"),
        None => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_have_hints() {
        for name in [
            "edit",
            "write",
            "shell_execute",
            "http_request",
            "memory_store",
            "calendar_create",
            "send_telegram",
            "email_send",
            "web_fetch",
        ] {
            assert!(hint_for(name).is_some(), "expected hint for {name}");
        }
    }

    #[test]
    fn unknown_tool_returns_none() {
        assert!(hint_for("never_heard_of_this_tool").is_none());
    }

    #[test]
    fn hints_are_short_enough() {
        // Cap at ~600 chars so even the longest hint stays cheap to
        // re-ship on every failure of that tool. (Many failures means
        // the loop guard fires anyway.)
        for name in [
            "edit",
            "write",
            "read",
            "shell_execute",
            "http_request",
            "memory_store",
            "memory_recall",
            "calendar_create",
            "send_telegram",
            "email_send",
            "web_search",
            "web_fetch",
        ] {
            let h = hint_for(name).unwrap();
            assert!(h.len() < 600, "hint for {name} too long: {}", h.len());
            assert!(h.len() > 40, "hint for {name} too short: {}", h.len());
        }
    }

    #[test]
    fn maybe_append_wraps_known_tool() {
        let out = maybe_append_hint("Error: file not found", "edit", None);
        assert!(out.contains("Error: file not found"));
        assert!(out.contains("<system-reminder>"));
        assert!(out.contains("prior read"));
        assert!(out.contains("</system-reminder>"));
    }

    #[test]
    fn maybe_append_passes_through_unknown_tool() {
        let out = maybe_append_hint("Error: something", "mystery_tool", None);
        assert_eq!(out, "Error: something");
    }

    #[test]
    fn maybe_append_skips_loop_guard() {
        let out = maybe_append_hint("Error: looped", "edit", Some("loop_guard"));
        assert_eq!(out, "Error: looped");
        assert!(!out.contains("<system-reminder>"));
    }

    #[test]
    fn maybe_append_skips_duplicate_in_batch() {
        let out = maybe_append_hint("Error: dup", "shell_execute", Some("duplicate_in_batch"));
        assert_eq!(out, "Error: dup");
    }

    #[test]
    fn maybe_append_appends_for_normal_tool_failure() {
        let out = maybe_append_hint(
            r#"{"success":false,"error":"bad endpoint"}"#,
            "http_request",
            Some("invalid_endpoint_id"),
        );
        assert!(out.contains("<system-reminder>"));
        assert!(out.contains("UUID"));
    }
}
