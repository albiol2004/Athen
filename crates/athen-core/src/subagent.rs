//! Canonical names for the sub-agent spawning tool.
//!
//! The tool was originally shipped as `delegate_to_agent`. It is now
//! surfaced to the LLM as `spawn_subagent` (the cross-harness term — Claude
//! Code's `Task`/`subagent_type`, OpenAI's `transfer_to_*`, Antigravity's
//! "subagents"), which also matches Athen's own internal
//! `AgentSource::Subagent` classification. The old name is kept as a
//! back-compat alias so prompts, transcripts, and any model that learned the
//! original name keep working: `list_tools` advertises only the new name, but
//! `call_tool` accepts either.
//!
//! Constants live in `athen-core` so both the executor (athen-agent, which
//! force-includes the tool past every profile's `ToolSelection`) and the
//! delegation registry (athen-app) reference one source of truth.

/// The name the LLM sees and should emit.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// The pre-rename name, still accepted on the wire.
pub const SPAWN_SUBAGENT_LEGACY_ALIAS: &str = "delegate_to_agent";

/// True if `name` refers to the sub-agent tool under either its current
/// name or its legacy alias.
pub fn is_spawn_subagent_name(name: &str) -> bool {
    name == SPAWN_SUBAGENT_TOOL_NAME || name == SPAWN_SUBAGENT_LEGACY_ALIAS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_both_names() {
        assert!(is_spawn_subagent_name("spawn_subagent"));
        assert!(is_spawn_subagent_name("delegate_to_agent"));
        assert!(!is_spawn_subagent_name("delegate"));
        assert!(!is_spawn_subagent_name("shell_execute"));
        assert!(!is_spawn_subagent_name(""));
    }
}
