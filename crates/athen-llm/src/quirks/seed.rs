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
        ModelFamily::Gemini3Pro => "gemini-3.1-pro",
        ModelFamily::Gemini3Flash => "gemini-3-flash",
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
}
