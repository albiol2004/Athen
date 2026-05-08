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
        ModelFamily::Gpt5 => "gpt-5",
        ModelFamily::OpenAiO3 => "o3",
        ModelFamily::Gemini3Pro => "gemini-3-pro",
        ModelFamily::DeepSeekV4Chat => "deepseek-chat",
        ModelFamily::DeepSeekR1 => "deepseek-reasoner",
        ModelFamily::Qwen35Local => "qwen3.5-9b-instruct",
        ModelFamily::Qwen36Local => "qwen3.6-27b-instruct",
        ModelFamily::Gemma4Local => "gemma-4-27b-it",
        ModelFamily::KimiK26Cloud => "kimi-k2-6",
        ModelFamily::MiniMaxM25Cloud => "minimax-m2-5",
        ModelFamily::Llama32Instruct => "llama-3.2-90b-instruct",
        ModelFamily::Llama4Instruct => "llama-4-maverick-instruct",
        ModelFamily::MistralLarge3 => "mistral-large-latest",
        ModelFamily::MagistralMedium => "magistral-medium-latest",
        ModelFamily::Codestral2508 => "codestral-latest",
    }
}

/// Resolve a `ModelFamily` to the `ModelQuirks` profile that drives
/// response parsing for that family's models. `ModelFamily::Default`
/// returns `ModelQuirks::default()` — every unprofiled family does the
/// same thing as today's executor.
pub fn quirks_for_family(family: ModelFamily) -> ModelQuirks {
    match family {
        ModelFamily::Default => ModelQuirks::default(),

        // Claude family: native typed `thinking` blocks; structured tool calls.
        ModelFamily::ClaudeOpus47 | ModelFamily::ClaudeSonnet46 | ModelFamily::ClaudeHaiku45 => {
            ModelQuirks {
                reasoning_surface: ReasoningSurface::NativeContentBlock,
                ..ModelQuirks::default()
            }
        }

        // OpenAI: o-series hides reasoning server-side; GPT-5 ditto.
        ModelFamily::Gpt5 | ModelFamily::OpenAiO3 => ModelQuirks {
            reasoning_surface: ReasoningSurface::HiddenServerSide,
            ..ModelQuirks::default()
        },

        // Gemini: native `part.thought:true` content blocks.
        ModelFamily::Gemini3Pro => ModelQuirks {
            reasoning_surface: ReasoningSurface::NativeContentBlock,
            ..ModelQuirks::default()
        },

        // DeepSeek-V4 chat: structured tool calls, no reasoning, but
        // streaming control-char repair is needed.
        ModelFamily::DeepSeekV4Chat => ModelQuirks {
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

        // Qwen 3.5 / 3.6 (local): inline XML tool calls, inline think tags,
        // strict template (system must be first).
        ModelFamily::Qwen35Local | ModelFamily::Qwen36Local => ModelQuirks {
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

        // Llama 3.2 / 4 instruct: inline JSON-array tool calls.
        ModelFamily::Llama32Instruct | ModelFamily::Llama4Instruct => ModelQuirks {
            tool_extraction: ToolExtractionStrategy::InlineJsonLlama,
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
    fn claude_quirks_use_native_thinking_blocks() {
        for fam in [
            ModelFamily::ClaudeOpus47,
            ModelFamily::ClaudeSonnet46,
            ModelFamily::ClaudeHaiku45,
        ] {
            let q = quirks_for_family(fam);
            assert_eq!(q.reasoning_surface, ReasoningSurface::NativeContentBlock);
            assert_eq!(q.tool_extraction, ToolExtractionStrategy::Structured);
        }
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
