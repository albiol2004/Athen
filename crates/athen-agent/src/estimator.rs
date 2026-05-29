//! Static-prefix size estimator for an agent profile.
//!
//! Surfaced in the UI as "this profile costs ~X tokens at fresh start"
//! so users can pick a profile knowing what their bill looks like before
//! they spend a turn. Strict requirement: the numbers MUST come from the
//! same builders the executor uses, so they can't drift as the prompt
//! evolves. See `executor::build_system_prompt_with_mode` — that is the
//! single source of truth, and we call it directly here rather than
//! re-implementing the section assembly.
//!
//! The reported numbers are character counts plus a coarse char→token
//! heuristic. We deliberately do NOT pull in `tiktoken` (or any other
//! provider-specific tokenizer) — every provider tokenizes differently,
//! a real tokenizer would only be exact for one of them, and the chip
//! is a rough budgeting hint, not pricing.

use std::collections::HashSet;
use std::path::Path;

use athen_core::agent_profile::ResolvedAgentProfile;
use athen_core::tool::ToolDefinition;

use crate::executor::DefaultExecutor;
use crate::tool_grouping::is_always_revealed;
use crate::toolbox::ToolboxPromptInfo;

/// Character-count breakdown of the static prompt prefix.
///
/// `system_prompt` is what `build_system_prompt_with_mode` returns;
/// `tools_array` approximates the JSON schema payload providers also
/// see in the request body for the always-revealed tool slice.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PromptCharsBreakdown {
    /// Chars of the rendered system prompt (header + identity +
    /// workspace + shell + toolbox + endpoints + tool index + persona
    /// rules + revealed-tool schemas — i.e. everything except the
    /// per-turn volatile suffix that rides in the first user message).
    pub system_prompt: usize,
    /// Sum of `serde_json::to_string(...).len()` over the always-revealed
    /// tool definitions. Approximates what the provider sees in the
    /// request body's `tools` array — close enough for budgeting; we
    /// don't try to mirror provider-specific schema reformatting.
    pub tools_array: usize,
    /// `system_prompt + tools_array`.
    pub total: usize,
}

/// Heuristic char→token estimate. ~4 chars/token for English prose,
/// ~3 chars/token for JSON. Mixed prompts (prose + tool schemas) land
/// somewhere between, so we use 3.7. Approximate; for UI guidance only.
pub fn approx_tokens(chars: usize) -> usize {
    (chars as f64 / 3.7).round() as usize
}

/// Estimate the static prefix size for an agent profile at fresh start.
///
/// `tools` should be the full tool slice the host would hand to the
/// executor (i.e. everything the per-arc registry produces). This
/// function applies the profile's `tool_selection` filter, picks the
/// always-revealed subset, and feeds the same argument shape into
/// `build_system_prompt_with_mode` that the executor uses on iteration
/// one — so the result is byte-equivalent to the prompt the runtime
/// would build before the agent reveals any new tools.
///
/// Pass `autonomous = false` for the user-facing chip: the chip
/// represents the profile's cost when the user picks it from the UI,
/// which is always the interactive surface. Sense-driven autonomous
/// runs reuse the same profile + tools but emit a slightly different
/// prompt; we don't surface that variant separately.
#[allow(clippy::too_many_arguments)]
pub fn estimate_static_prompt_chars(
    tools: &[ToolDefinition],
    profile: Option<&ResolvedAgentProfile>,
    identity_block: Option<&str>,
    endpoints_block: Option<&str>,
    skills_block: Option<&str>,
    mission_block: Option<&str>,
    toolbox_info: Option<&ToolboxPromptInfo>,
    shell_kind: Option<&'static str>,
    tool_doc_dir: Option<&Path>,
    has_context: bool,
    autonomous: bool,
) -> PromptCharsBreakdown {
    // Apply the profile's tool selection so the estimate matches what
    // the executor would actually see for this profile.
    let available: Vec<ToolDefinition> = match profile {
        Some(p) => crate::executor::apply_tool_selection(tools, &p.profile.tool_selection),
        None => tools.to_vec(),
    };

    // Fresh-start "revealed" set = always-revealed subset of available.
    // The executor reveals more as the agent calls new tools, but at
    // turn zero this is the universe. The chip is a fresh-start hint.
    let revealed: HashSet<String> = available
        .iter()
        .filter(|t| is_always_revealed(&t.name))
        .map(|t| t.name.clone())
        .collect();

    let system_prompt = DefaultExecutor::build_system_prompt_with_mode(
        &available,
        &revealed,
        has_context,
        tool_doc_dir,
        profile,
        toolbox_info,
        shell_kind,
        autonomous,
        identity_block,
        endpoints_block,
        skills_block,
        mission_block,
    );
    let system_prompt_chars = system_prompt.len();

    let tools_array_chars: usize = available
        .iter()
        .filter(|t| revealed.contains(&t.name))
        .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
        .sum();

    PromptCharsBreakdown {
        system_prompt: system_prompt_chars,
        tools_array: tools_array_chars,
        total: system_prompt_chars + tools_array_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::agent_profile::{
        AgentProfile, ExpertiseDeclaration, ResolvedAgentProfile, ToolSelection,
    };
    use athen_core::risk::BaseImpact;
    use athen_core::tool::{ToolBackend, ToolDefinition};
    use chrono::Utc;
    use serde_json::json;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("Description for {name} (test fixture)."),
            parameters: json!({
                "type": "object",
                "properties": { "x": { "type": "string" } },
                "required": ["x"],
            }),
            backend: ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: BaseImpact::Read,
        }
    }

    /// 31-ish tools spanning every group the runtime exposes. The exact
    /// count and naming match `tool_grouping::is_always_revealed` so the
    /// "always revealed" subset comes out right.
    fn realistic_tools() -> Vec<ToolDefinition> {
        vec![
            // Always-revealed core (memory + shell + files + web + email_send)
            def("memory_store"),
            def("memory_recall"),
            def("shell_execute"),
            def("shell_spawn"),
            def("shell_kill"),
            def("shell_logs"),
            def("read"),
            def("edit"),
            def("write"),
            def("grep"),
            def("list_directory"),
            def("web_search"),
            def("web_fetch"),
            def("email_send"),
            // Tier-2 tools that show up in the index but not as inline schemas
            def("calendar_create"),
            def("calendar_list"),
            def("calendar_update"),
            def("calendar_delete"),
            def("contacts_search"),
            def("contacts_upsert"),
            def("contacts_delete"),
            def("identity_add"),
            def("identity_search"),
            def("read_attachment_full"),
            def("fetch_attachment"),
            def(athen_core::subagent::SPAWN_SUBAGENT_TOOL_NAME),
            def("create_wakeup"),
            def("update_wakeup"),
            def("delete_wakeup"),
            def("list_wakeups"),
            def("http_request"),
        ]
    }

    fn default_profile() -> ResolvedAgentProfile {
        let now = Utc::now();
        ResolvedAgentProfile {
            profile: AgentProfile {
                id: AgentProfile::DEFAULT_ID.into(),
                display_name: "Athen (default)".into(),
                description: String::new(),
                persona_template_ids: vec![],
                custom_persona_addendum: None,
                tool_selection: ToolSelection::All,
                primary_groups: vec![],
                expertise: ExpertiseDeclaration::default(),
                model_profile_hint: None,
                github_identity: athen_core::agent_profile::GithubIdentity::None,
                builtin: true,
                created_at: now,
                updated_at: now,
            },
            persona_templates: vec![],
        }
    }

    fn coder_profile() -> ResolvedAgentProfile {
        let now = Utc::now();
        ResolvedAgentProfile {
            profile: AgentProfile {
                id: "coder".into(),
                display_name: "Coder".into(),
                description: "Coding-focused subset.".into(),
                persona_template_ids: vec![],
                custom_persona_addendum: None,
                tool_selection: ToolSelection::Groups(vec![
                    "shell".into(),
                    "files".into(),
                    "memory".into(),
                    "web".into(),
                ]),
                primary_groups: vec![],
                expertise: ExpertiseDeclaration::default(),
                model_profile_hint: None,
                github_identity: athen_core::agent_profile::GithubIdentity::None,
                builtin: true,
                created_at: now,
                updated_at: now,
            },
            persona_templates: vec![],
        }
    }

    #[test]
    fn estimate_default_profile_in_plausible_range() {
        let tools = realistic_tools();
        let profile = default_profile();
        let identity = "## personality\nBe concise.\n\n## rules\nNever auto-send to legal@.";
        let est = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            Some(identity),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        // Loose bounds: this is a sanity check, not a fingerprint.
        assert!(
            est.total > 4_000 && est.total < 50_000,
            "default total chars out of expected range: {}",
            est.total
        );
        assert!(est.system_prompt > 0);
        assert!(est.tools_array > 0);
        assert_eq!(est.total, est.system_prompt + est.tools_array);
    }

    #[test]
    fn coder_profile_smaller_than_default() {
        let tools = realistic_tools();
        let default = default_profile();
        let coder = coder_profile();
        let d = estimate_static_prompt_chars(
            &tools,
            Some(&default),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        let c = estimate_static_prompt_chars(
            &tools,
            Some(&coder),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        // Coder narrows tool_selection to a few groups → smaller prompt.
        assert!(
            c.total < d.total,
            "coder ({}) should be smaller than default ({})",
            c.total,
            d.total
        );
    }

    #[test]
    fn identity_block_increases_total_monotonically() {
        let tools = realistic_tools();
        let profile = default_profile();
        let no_identity = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        let small_identity = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            Some("## personality\nBe terse."),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        let big_identity_str = format!("## personality\n{}", "Be terse. ".repeat(200));
        let big_identity = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            Some(&big_identity_str),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        assert!(no_identity.total < small_identity.total);
        assert!(small_identity.total < big_identity.total);
    }

    #[test]
    fn endpoints_block_ignored_when_http_request_absent() {
        // Build a tool slice WITHOUT http_request — endpoints section
        // is gated on `http_request` being present in the slice.
        let tools: Vec<ToolDefinition> = realistic_tools()
            .into_iter()
            .filter(|t| t.name != "http_request")
            .collect();
        let profile = default_profile();
        let with_endpoints = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            None,
            Some("- **ElevenLabs** (https://api.elevenlabs.io/v1/) — text-to-speech."),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        let without_endpoints = estimate_static_prompt_chars(
            &tools,
            Some(&profile),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        );
        // Both must be byte-equal — gate dropped the endpoints block.
        assert_eq!(with_endpoints.total, without_endpoints.total);
    }

    #[test]
    fn print_ballpark_for_human_inspection() {
        // Not an assertion-heavy test — just dumps the numbers so a
        // human can compare against the headline figures cited in
        // issue #204 (default ~6.2k tok, coder ~4.8k tok). Run with
        // `cargo test -p athen-agent estimator -- --nocapture`.
        let tools = realistic_tools();
        let identity = "## personality\nBe warm but concise.\n\n## rules\nNever auto-send to legal@.\n## knowledge\nThe user runs Fedora 44 with KDE Plasma.";
        let endpoints = "- **ElevenLabs** (https://api.elevenlabs.io/v1/) — text-to-speech.";

        let default_est = estimate_static_prompt_chars(
            &tools,
            Some(&default_profile()),
            Some(identity),
            Some(endpoints),
            None,
            None,
            None,
            Some("nushell"),
            None,
            false,
            false,
        );
        let coder_est = estimate_static_prompt_chars(
            &tools,
            Some(&coder_profile()),
            Some(identity),
            Some(endpoints),
            None,
            None,
            None,
            Some("nushell"),
            None,
            false,
            false,
        );
        eprintln!(
            "[estimator ballpark] default: total_chars={} tokens={} (sys={} tools={})",
            default_est.total,
            approx_tokens(default_est.total),
            default_est.system_prompt,
            default_est.tools_array,
        );
        eprintln!(
            "[estimator ballpark] coder:   total_chars={} tokens={} (sys={} tools={})",
            coder_est.total,
            approx_tokens(coder_est.total),
            coder_est.system_prompt,
            coder_est.tools_array,
        );
        // Sanity: both > 1k tokens (we have a substantial system
        // prompt with workspace rules + tool index).
        assert!(approx_tokens(default_est.total) > 1_000);
        assert!(approx_tokens(coder_est.total) > 1_000);
    }

    #[test]
    fn approx_tokens_matches_4_chars_per_token_ish() {
        // Sanity: 3700 chars → ~1000 tokens (within 1).
        assert!((approx_tokens(3_700) as i64 - 1_000).abs() <= 1);
        assert_eq!(approx_tokens(0), 0);
    }
}
