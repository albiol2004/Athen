//! Seed table mapping `ModelFamily` to its `ModelQuirks` profile and
//! default model slug. This is the code mirror of the table in
//! `docs/PER_MODEL_QUIRKS.md` §5. Adding a new family is one row here +
//! one variant in `athen_core::llm::ModelFamily` + a UI dropdown label.

use athen_core::llm::ModelFamily;

use super::{
    ModelQuirks, ReasoningSurface, TemplateStrictness, ToolArgRepair, ToolExtractionStrategy,
};

/// The default slug Athen pre-fills into the provider's "Model slug" UI
/// field when the user picks a family. The user is free to edit it
/// afterwards (e.g. to point at a dated or fine-tuned variant with the
/// same wire format).
pub fn default_slug_for_family(family: ModelFamily) -> &'static str {
    match family {
        ModelFamily::Default => "",
        ModelFamily::ClaudeOpus47 => "claude-opus-4-7",
        ModelFamily::ClaudeSonnet46 => "claude-sonnet-4-6",
        ModelFamily::ClaudeHaiku45 => "claude-haiku-4-5",
        ModelFamily::Gpt5 => "gpt-5.5",
        ModelFamily::OpenAiO3 => "o4-mini",
        // Gemini 3 family is preview-only on the Generative Language API
        // as of May 2026 — the `-preview` suffix is mandatory or the API
        // 404s the model. Pro is at 3.1 (older 3.0 Pro Preview was shut
        // down 2026-03-09); Flash is still on the 3.0 lineage.
        ModelFamily::Gemini3Pro => "gemini-3.1-pro-preview",
        ModelFamily::Gemini3Flash => "gemini-3-flash-preview",
        // `deepseek-chat` is a backward-compatible alias still accepted by
        // DeepSeek's API; the new canonical (2026) is `deepseek-v4-flash`.
        ModelFamily::DeepSeekV4Chat => "deepseek-v4-flash",
        ModelFamily::DeepSeekR1 => "deepseek-reasoner",
        ModelFamily::Qwen35Local => "qwen3.5-9b-instruct",
        ModelFamily::Qwen36Local => "qwen3.6-27b-instruct",
        ModelFamily::Gemma4Local => "gemma-4-27b-it",
        ModelFamily::KimiK26Cloud => "kimi-k2-6",
        ModelFamily::MiniMaxM25Cloud => "minimax-m2-5",
        ModelFamily::Llama32Instruct => "llama-3.2-90b-vision-instruct",
        ModelFamily::Llama33Instruct => "meta-llama/Llama-3.3-70B-Instruct",
        ModelFamily::Llama4Instruct => "llama-4-maverick-17b-128e-instruct",
        ModelFamily::MistralLarge3 => "mistral-large-latest",
        ModelFamily::MagistralMedium => "magistral-medium-latest",
        ModelFamily::Codestral2508 => "codestral-latest",
        ModelFamily::DeepSeekV4Pro => "deepseek-v4-pro",
        ModelFamily::Qwen3CoderNext => "Qwen/Qwen3-Coder-Next",
        ModelFamily::Grok4 => "grok-4",
    }
}

/// Resolve a `ModelFamily` to the `ModelQuirks` profile that drives
/// response parsing for that family's models. `ModelFamily::Default`
/// returns `ModelQuirks::default()` — every unprofiled family does the
/// same thing as today's executor.
pub fn quirks_for_family(family: ModelFamily) -> ModelQuirks {
    match family {
        ModelFamily::Default => ModelQuirks::default(),

        // Claude Opus 4.7 / Sonnet 4.6: native typed `thinking` blocks
        // (extended / adaptive thinking) + structured tool calls.
        ModelFamily::ClaudeOpus47 | ModelFamily::ClaudeSonnet46 => ModelQuirks {
            reasoning_surface: ReasoningSurface::NativeContentBlock,
            ..ModelQuirks::default()
        },

        // Claude Haiku 4.5: structured tool calls, NO extended thinking
        // support (Haiku 4.5 has no thinking-block API). Resolves to default
        // baseline quirks.
        ModelFamily::ClaudeHaiku45 => ModelQuirks::default(),

        // OpenAI chat models (GPT-5 / 5.4 / 5.5 chat / GPT-5.5 Pro / Codex):
        // structured tool calls, no exposed reasoning. The o-series (o3 /
        // o4-mini) hides reasoning server-side and only emits token counts.
        ModelFamily::Gpt5 => ModelQuirks::default(),
        ModelFamily::OpenAiO3 => ModelQuirks {
            reasoning_surface: ReasoningSurface::HiddenServerSide,
            ..ModelQuirks::default()
        },

        // Gemini: native `part.thought:true` content blocks. Pro and Flash
        // share the same wire format — the family split is for the UI slug
        // dropdown and (eventually) per-tier cost estimation.
        ModelFamily::Gemini3Pro | ModelFamily::Gemini3Flash => ModelQuirks {
            reasoning_surface: ReasoningSurface::NativeContentBlock,
            ..ModelQuirks::default()
        },

        // DeepSeek V4 chat / V4 Pro (non-thinking mode): structured tool
        // calls, no reasoning surface, control-char repair on streaming
        // tool args. If the user enables thinking mode on either model,
        // they should pick `DeepSeekR1` instead (same wire shape).
        ModelFamily::DeepSeekV4Chat | ModelFamily::DeepSeekV4Pro => ModelQuirks {
            tool_arg_repair: ToolArgRepair {
                control_chars_to_unicode_escape: true,
                ..ToolArgRepair::empty()
            },
            ..ModelQuirks::default()
        },

        // DeepSeek-R1: reasoning_content separate field; reasoning must
        // be echoed back on the next turn after a tool call.
        ModelFamily::DeepSeekR1 => ModelQuirks {
            reasoning_surface: ReasoningSurface::SeparateField,
            tool_arg_repair: ToolArgRepair {
                control_chars_to_unicode_escape: true,
                ..ToolArgRepair::empty()
            },
            echo_reasoning_on_tool_turn: true,
            ..ModelQuirks::default()
        },

        // Qwen 3.5 / 3.6 (local): tool calls inside `<tool_call>` wrappers
        // — Hermes-JSON form is most common with llama.cpp `--jinja`, with
        // legacy Qwen-XML as fallback. The extractor handles both. Inline
        // think tags + strict template (system must be first).
        ModelFamily::Qwen35Local | ModelFamily::Qwen36Local => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlineXmlQwenStyle,
            reasoning_surface: ReasoningSurface::InlineThinkTags,
            template_strictness: TemplateStrictness::SystemMustBeFirst,
            ..ModelQuirks::default()
        },

        // Qwen3-Coder Next (Feb 2026): inline XML tool calls in a different
        // shape: `<TOOL_NAME><parameter=KEY>VAL</parameter></TOOL_NAME>`.
        // Today this maps onto the Qwen-style extractor as a best-effort
        // fallback; a dedicated `InlineXmlQwenCoderStyle` variant is
        // tracked for slice 5 when we have a captured payload to test
        // against.
        ModelFamily::Qwen3CoderNext => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlineXmlQwenStyle,
            reasoning_surface: ReasoningSurface::InlineThinkTags,
            template_strictness: TemplateStrictness::SystemMustBeFirst,
            ..ModelQuirks::default()
        },

        // Gemma 4 (local): inline think tags, strict template, system msg
        // required, Ollama double-encodes JSON arrays.
        ModelFamily::Gemma4Local => ModelQuirks {
            reasoning_surface: ReasoningSurface::InlineThinkTags,
            template_strictness: TemplateStrictness::SystemMustBeFirst,
            tool_arg_repair: ToolArgRepair {
                unescape_double_encoded_json_arrays: true,
                ..ToolArgRepair::empty()
            },
            system_message_required: true,
            ..ModelQuirks::default()
        },

        // Kimi K2.6 cloud: reasoning_content separate field.
        ModelFamily::KimiK26Cloud => ModelQuirks {
            reasoning_surface: ReasoningSurface::SeparateField,
            ..ModelQuirks::default()
        },

        // MiniMax M2.5 cloud: vendor-tagged inline XML, inline think tags.
        ModelFamily::MiniMaxM25Cloud => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlineXmlVendorTagged("minimax"),
            reasoning_surface: ReasoningSurface::InlineThinkTags,
            ..ModelQuirks::default()
        },

        // Llama 3.2 (Vision / 70B class) and Llama 3.3 70B instruct: inline
        // JSON-array tool calls `[{"name":..., "parameters":{...}}]`.
        // (Llama 3.2 1B / 3B emit pythonic instead — see below.)
        ModelFamily::Llama32Instruct | ModelFamily::Llama33Instruct => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlineJsonLlama,
            ..ModelQuirks::default()
        },

        // Llama 4 Scout / Maverick: pythonic tool calls
        // `[func(p=v, q=w)]`. This shape is what the upstream
        // `llama4_pythonic` parser in vLLM emits and what llama.cpp
        // surfaces when running Llama 4 GGUF builds.
        ModelFamily::Llama4Instruct => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlinePythonicLlama,
            ..ModelQuirks::default()
        },

        // Mistral Large 3: cloud uses structured calls; SystemAbsorbedIntoUser
        // for the [INST] template. Local-only `[TOOL_CALLS]` extraction is
        // a future addition (different family row when we ship it).
        ModelFamily::MistralLarge3 => ModelQuirks {
            template_strictness: TemplateStrictness::SystemAbsorbedIntoUser,
            ..ModelQuirks::default()
        },

        // Magistral: structured calls, inline think tags, [INST] template.
        ModelFamily::MagistralMedium => ModelQuirks {
            reasoning_surface: ReasoningSurface::InlineThinkTags,
            template_strictness: TemplateStrictness::SystemAbsorbedIntoUser,
            ..ModelQuirks::default()
        },

        // Codestral: structured calls, no reasoning, [INST] template.
        ModelFamily::Codestral2508 => ModelQuirks {
            template_strictness: TemplateStrictness::SystemAbsorbedIntoUser,
            ..ModelQuirks::default()
        },

        // xAI Grok 4 (Grok 4.3 lineage). OpenAI-compat structured tool
        // calls, no reasoning surface, lenient template, 1M ctx. No
        // wire-level quirks vs the OpenAI-compat baseline.
        ModelFamily::Grok4 => ModelQuirks::default(),
    }
}

// ---------------------------------------------------------------------------
// Per-slug quirks registry (Bundles Phase 3)
// ---------------------------------------------------------------------------
//
// Until Bundles, the wire-format `ModelFamily` was selected per-Connection.
// That falls apart the moment one Connection hosts slugs spanning multiple
// wire families (OpenCode Go relays DeepSeek + Qwen + Kimi + GLM + MiMo via
// OpenAI-compat AND MiniMax M2.x via Anthropic-compat under one provider id;
// OpenRouter likewise multiplexes vendors). The registry below moves the
// lookup key from `connection_id` alone to `(connection_id, slug)`, so each
// tier in a Bundle resolves to its own correct family.
//
// Today's `SlugQuirks` carries only `family` — the field that already gates
// every quirks branch in `apply_to_response`. `default_reasoning` and
// `catalog_label` (see `docs/BUNDLES.md` §"Per-slug quirks registry") are
// follow-ups; adding fields is non-breaking because lookups return `Option`
// and callers fall back to the Connection's own family for unknown slugs.

/// Per-slug wire-format quirks override. Resolved at provider-construction
/// time inside `build_provider_instance`. When a slug appears in
/// `BUILTIN_SLUG_QUIRKS`, its family wins over whatever family the
/// Connection itself was configured with — necessary for relays
/// (`opencode_go`, `openrouter`) that host slugs from multiple wire
/// families behind one credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlugQuirks {
    pub family: ModelFamily,
}

/// `(connection_pattern, slug_pattern, family)` entries — first match wins,
/// order matters. Patterns are case-insensitive.
///
/// - `connection_pattern == "*"` matches any connection id (use sparingly,
///   only for slugs whose wire shape is unambiguous regardless of which
///   relay serves them — e.g. `claude-*` is always the Anthropic wire).
/// - `slug_pattern` ending in `*` is a prefix match (e.g. `claude-opus-4-7*`
///   matches dated variants `claude-opus-4-7-20260301`).
/// - Otherwise both patterns are exact (case-insensitive) matches.
///
/// Adding a row here is the supported way to teach Athen about a new slug.
/// A `None` lookup result is fine — the caller falls back to the
/// Connection's `family` field, preserving today's behaviour for slugs the
/// registry has never seen (typed via the "Custom" escape hatch).
const BUILTIN_SLUG_QUIRKS: &[(&str, &str, ModelFamily)] = &[
    // --- Direct providers ---------------------------------------------------
    ("deepseek", "deepseek-chat", ModelFamily::DeepSeekV4Chat),
    ("deepseek", "deepseek-v4-flash", ModelFamily::DeepSeekV4Chat),
    ("deepseek", "deepseek-v4-pro", ModelFamily::DeepSeekV4Pro),
    ("deepseek", "deepseek-reasoner", ModelFamily::DeepSeekR1),
    ("deepseek", "deepseek-r1*", ModelFamily::DeepSeekR1),
    ("anthropic", "claude-opus-4-7*", ModelFamily::ClaudeOpus47),
    (
        "anthropic",
        "claude-sonnet-4-6*",
        ModelFamily::ClaudeSonnet46,
    ),
    ("anthropic", "claude-haiku-4-5*", ModelFamily::ClaudeHaiku45),
    ("openai", "gpt-5*", ModelFamily::Gpt5),
    ("openai", "o3*", ModelFamily::OpenAiO3),
    ("openai", "o4-mini*", ModelFamily::OpenAiO3),
    ("google", "gemini-3.1-pro*", ModelFamily::Gemini3Pro),
    ("google", "gemini-3-pro*", ModelFamily::Gemini3Pro),
    ("google", "gemini-3-flash*", ModelFamily::Gemini3Flash),
    ("google", "gemini-3.0-flash*", ModelFamily::Gemini3Flash),
    (
        "minimax_anthropic",
        "minimax-m2*",
        ModelFamily::MiniMaxM25Cloud,
    ),
    // --- OpenCode Go relay (per-slug wire dispatch) ------------------------
    // The provider adapter is selected by `is_minimax_slug` in
    // `build_provider_instance`; the family below drives apply_to_response
    // post-processing for each slug.
    (
        "opencode_go",
        "deepseek-v4-flash",
        ModelFamily::DeepSeekV4Chat,
    ),
    ("opencode_go", "deepseek-v4-pro", ModelFamily::DeepSeekV4Pro),
    ("opencode_go", "deepseek-chat", ModelFamily::DeepSeekV4Chat),
    ("opencode_go", "kimi-k2*", ModelFamily::KimiK26Cloud),
    ("opencode_go", "qwen3-coder*", ModelFamily::Qwen3CoderNext),
    ("opencode_go", "minimax-m2*", ModelFamily::MiniMaxM25Cloud),
    // glm-4.6 and mimo-7b have no profiled family yet — they fall through
    // to None and inherit the Connection's family (Default for opencode_go,
    // which yields baseline quirks).

    // --- Cross-provider fallbacks ------------------------------------------
    // Any connection serving these well-known slugs (e.g. an OpenRouter
    // Connection routing claude-* / gemini-*) gets the right family without
    // a per-connection row.
    ("*", "claude-opus-4-7*", ModelFamily::ClaudeOpus47),
    ("*", "claude-sonnet-4-6*", ModelFamily::ClaudeSonnet46),
    ("*", "claude-haiku-4-5*", ModelFamily::ClaudeHaiku45),
    ("*", "gemini-3.1-pro*", ModelFamily::Gemini3Pro),
    ("*", "gemini-3-pro*", ModelFamily::Gemini3Pro),
    ("*", "gemini-3-flash*", ModelFamily::Gemini3Flash),
    ("*", "gpt-5*", ModelFamily::Gpt5),
];

/// Look up per-slug quirks. Returns `None` when no row matches — callers
/// should fall back to the Connection's own `family` field, which itself
/// defaults to `ModelFamily::Default` (baseline behaviour).
///
/// Connection ids and slugs are matched case-insensitively. Slugs ending
/// in `*` in the table match the literal prefix.
pub fn lookup_slug_quirks(connection_id: &str, slug: &str) -> Option<SlugQuirks> {
    let slug_lower = slug.to_ascii_lowercase();
    let conn_lower = connection_id.to_ascii_lowercase();
    for (conn_pat, slug_pat, family) in BUILTIN_SLUG_QUIRKS {
        let conn_match = *conn_pat == "*" || conn_pat.eq_ignore_ascii_case(&conn_lower);
        if !conn_match {
            continue;
        }
        let pat_lower = slug_pat.to_ascii_lowercase();
        let slug_match = if let Some(prefix) = pat_lower.strip_suffix('*') {
            slug_lower.starts_with(prefix)
        } else {
            pat_lower == slug_lower
        };
        if slug_match {
            return Some(SlugQuirks { family: *family });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_family_yields_default_quirks() {
        assert_eq!(
            quirks_for_family(ModelFamily::Default),
            ModelQuirks::default()
        );
    }

    #[test]
    fn default_family_has_empty_slug() {
        assert_eq!(default_slug_for_family(ModelFamily::Default), "");
    }

    #[test]
    fn every_family_has_a_seed_and_a_slug() {
        // Touches every variant and asserts non-default families have a
        // non-empty slug. If a new family is added without a row here, the
        // match in `quirks_for_family` will fail to compile (exhaustive),
        // which is the point.
        for family in ModelFamily::all() {
            let _quirks = quirks_for_family(*family);
            if *family != ModelFamily::Default {
                assert!(
                    !default_slug_for_family(*family).is_empty(),
                    "non-default family {:?} must have a default slug",
                    family
                );
            }
        }
    }

    #[test]
    fn qwen_local_quirks_match_design_doc() {
        let q = quirks_for_family(ModelFamily::Qwen35Local);
        assert_eq!(
            q.tool_extraction,
            ToolExtractionStrategy::InlineXmlQwenStyle
        );
        assert_eq!(q.reasoning_surface, ReasoningSurface::InlineThinkTags);
        assert_eq!(q.template_strictness, TemplateStrictness::SystemMustBeFirst);
    }

    #[test]
    fn deepseek_r1_quirks_match_design_doc() {
        let q = quirks_for_family(ModelFamily::DeepSeekR1);
        assert_eq!(q.reasoning_surface, ReasoningSurface::SeparateField);
        assert!(q.echo_reasoning_on_tool_turn);
        assert!(q.tool_arg_repair.control_chars_to_unicode_escape);
    }

    #[test]
    fn claude_thinking_models_use_native_content_blocks() {
        // Opus 4.7 / Sonnet 4.6 expose extended (adaptive) thinking via
        // Anthropic native typed `thinking` content blocks. Haiku 4.5
        // intentionally omits thinking — see `claude_haiku_45_has_no_reasoning`.
        for fam in [ModelFamily::ClaudeOpus47, ModelFamily::ClaudeSonnet46] {
            let q = quirks_for_family(fam);
            assert_eq!(q.reasoning_surface, ReasoningSurface::NativeContentBlock);
            assert_eq!(q.tool_extraction, ToolExtractionStrategy::Structured);
        }
    }

    #[test]
    fn claude_haiku_45_has_no_reasoning() {
        // Haiku 4.5 has no extended-thinking API. Quirks resolve to default
        // baseline.
        let q = quirks_for_family(ModelFamily::ClaudeHaiku45);
        assert_eq!(q, ModelQuirks::default());
    }

    #[test]
    fn deepseek_v4_pro_uses_control_char_repair_no_reasoning() {
        // V4 Pro non-thinking shares the V4 chat shape: structured tool
        // calls + control-char repair, no reasoning surface. (Thinking-mode
        // users pick `DeepSeekR1` instead — same wire shape.)
        let q = quirks_for_family(ModelFamily::DeepSeekV4Pro);
        assert_eq!(q.tool_extraction, ToolExtractionStrategy::Structured);
        assert_eq!(q.reasoning_surface, ReasoningSurface::None);
        assert!(q.tool_arg_repair.control_chars_to_unicode_escape);
    }

    #[test]
    fn llama_4_uses_pythonic_tool_calls() {
        // Llama 4 Scout / Maverick emit pythonic `[func(p=v)]`, NOT JSON
        // arrays — that's the upstream `llama4_pythonic` parser shape.
        let q = quirks_for_family(ModelFamily::Llama4Instruct);
        assert_eq!(
            q.tool_extraction,
            ToolExtractionStrategy::InlinePythonicLlama
        );
    }

    #[test]
    fn llama_33_uses_json_array_tool_calls() {
        let q = quirks_for_family(ModelFamily::Llama33Instruct);
        assert_eq!(q.tool_extraction, ToolExtractionStrategy::InlineJsonLlama);
    }

    #[test]
    fn grok_4_default_baseline() {
        // No wire-level quirks vs OpenAI baseline.
        assert_eq!(
            quirks_for_family(ModelFamily::Grok4),
            ModelQuirks::default()
        );
    }

    #[test]
    fn minimax_uses_vendor_tagged_xml() {
        let q = quirks_for_family(ModelFamily::MiniMaxM25Cloud);
        match q.tool_extraction {
            ToolExtractionStrategy::InlineXmlVendorTagged(prefix) => {
                assert_eq!(prefix, "minimax");
            }
            other => panic!("expected vendor-tagged XML, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Per-slug registry (Phase 3)
    // -----------------------------------------------------------------------

    #[test]
    fn slug_lookup_exact_match_on_opencode_go_deepseek() {
        // OpenCode Go's DeepSeek slugs resolve to the DeepSeek family even
        // though the Connection itself defaults to Default — this is the
        // bug Phase 3 exists to fix.
        let q = lookup_slug_quirks("opencode_go", "deepseek-v4-flash").unwrap();
        assert_eq!(q.family, ModelFamily::DeepSeekV4Chat);

        let q = lookup_slug_quirks("opencode_go", "deepseek-v4-pro").unwrap();
        assert_eq!(q.family, ModelFamily::DeepSeekV4Pro);
    }

    #[test]
    fn slug_lookup_picks_minimax_for_relay_anthropic_wire() {
        // Same Connection, different wire family — exactly the case that
        // breaks per-Connection quirks dispatch.
        let q = lookup_slug_quirks("opencode_go", "minimax-m2.7").unwrap();
        assert_eq!(q.family, ModelFamily::MiniMaxM25Cloud);

        let q = lookup_slug_quirks("opencode_go", "minimax-m2.5").unwrap();
        assert_eq!(q.family, ModelFamily::MiniMaxM25Cloud);
    }

    #[test]
    fn slug_lookup_is_case_insensitive_both_sides() {
        // User-typed casings — MiniMax-M2.7 was the original failure mode.
        let q = lookup_slug_quirks("OpenCode_Go", "MiniMax-M2.7").unwrap();
        assert_eq!(q.family, ModelFamily::MiniMaxM25Cloud);
        let q = lookup_slug_quirks("ANTHROPIC", "Claude-Opus-4-7").unwrap();
        assert_eq!(q.family, ModelFamily::ClaudeOpus47);
    }

    #[test]
    fn slug_lookup_prefix_match_handles_dated_variants() {
        // Anthropic publishes dated slugs (`claude-opus-4-7-20260301`);
        // the prefix pattern must catch them.
        let q = lookup_slug_quirks("anthropic", "claude-opus-4-7-20260301").unwrap();
        assert_eq!(q.family, ModelFamily::ClaudeOpus47);
    }

    #[test]
    fn slug_lookup_returns_none_for_unknown_slug() {
        // mimo-7b and glm-4.6 are deliberately not in the table — they fall
        // through so the caller can use the Connection's family. Same for
        // anything genuinely Custom.
        assert!(lookup_slug_quirks("opencode_go", "mimo-7b").is_none());
        assert!(lookup_slug_quirks("opencode_go", "glm-4.6").is_none());
        assert!(lookup_slug_quirks("anything", "totally-unknown-model").is_none());
    }

    #[test]
    fn slug_lookup_wildcard_connection_catches_well_known_slugs() {
        // An OpenRouter-style Connection (not in the per-connection rows)
        // still resolves claude-* / gemini-* / gpt-* via the "*" patterns.
        let q = lookup_slug_quirks("openrouter", "claude-sonnet-4-6").unwrap();
        assert_eq!(q.family, ModelFamily::ClaudeSonnet46);

        let q = lookup_slug_quirks("openrouter", "gemini-3.1-pro-preview").unwrap();
        assert_eq!(q.family, ModelFamily::Gemini3Pro);

        let q = lookup_slug_quirks("openrouter", "gpt-5.5").unwrap();
        assert_eq!(q.family, ModelFamily::Gpt5);
    }

    #[test]
    fn slug_lookup_per_connection_row_wins_over_wildcard() {
        // First-match-wins ordering: the per-connection `opencode_go` rows
        // are listed before the `*` rows, so they take precedence — even
        // when a `*` row would also match. Verified here so a future
        // reorder breaks loudly.
        let q = lookup_slug_quirks("opencode_go", "deepseek-v4-flash").unwrap();
        assert_eq!(q.family, ModelFamily::DeepSeekV4Chat);
    }
}
