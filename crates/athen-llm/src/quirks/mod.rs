//! Per-model quirks — a small typed table that records how each provider /
//! open-weights model differs from the OpenAI-compat baseline along five
//! orthogonal axes. The provider response parsers consult this table instead
//! of branching on model name.
//!
//! See `docs/PER_MODEL_QUIRKS.md` for the design rationale and seed-table
//! source-of-truth. **This module is the code mirror of that doc.** When
//! the doc and code disagree, the doc wins until one of them is updated.
//!
//! # Layout
//!
//! - `mod.rs` (this file) — the `ModelQuirks` struct and the five axis enums
//!   / sub-flag fields. `Default` reproduces today's behavior for unknown
//!   models.
//! - `seed.rs` — seed table mapping `ModelFamily` → `ModelQuirks` and
//!   `default_slug_for_family`.
//! - `extractors/` — concrete `ToolExtractionStrategy` implementations
//!   (slice 3 will populate this).

use athen_core::llm::LlmResponse;
use serde::{Deserialize, Serialize};

pub mod extractors;
pub mod repair;
pub mod seed;

/// How tool calls should be recovered from a model's response.
///
/// Most cloud providers use `Structured` — the response carries a populated
/// `tool_calls` field that we trust verbatim. Open-weights models running
/// behind llama.cpp's `--jinja` parser commonly emit tool calls *inline in
/// content text* in vendor-specific formats; the inline variants below say
/// how to recover them.
///
/// Every non-`Structured` strategy is responsible for both (a) extracting
/// the tool calls into synthetic `ToolCall`s and (b) stripping them from
/// `content` so the prose remains clean. Output flows through the same
/// `ToolArgRepair` pipeline as `Structured` calls — strategies must not
/// bypass repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExtractionStrategy {
    /// Trust `tool_calls` from the response. The OpenAI/Anthropic baseline.
    Structured,
    /// Qwen3.5 / Qwen3.6 inline form:
    /// `<tool_call><function=NAME><parameter=KEY>VAL</parameter></function></tool_call>`.
    /// Also handles the Hermes-JSON variant
    /// `<tool_call>{"name":..., "arguments":{...}}</tool_call>`.
    InlineXmlQwenStyle,
    /// Vendor-tagged XML, e.g. MiniMax's
    /// `<minimax:tool_call><invoke name=NAME><parameter name=KEY>VAL</parameter></invoke></minimax:tool_call>`.
    /// The `&'static str` is the namespace prefix (e.g. `"minimax"`).
    InlineXmlVendorTagged(&'static str),
    /// Llama 3.2 / 4 inline JSON-array form:
    /// `[{"name":"NAME", "parameters":{...}}]` in content.
    InlineJsonLlama,
    /// Llama 1B/3B pythonic form: `[func(p=v, q=w)]`.
    InlinePythonicLlama,
    /// Mistral local: `[TOOL_CALLS]` (or another fixed token) followed by
    /// JSON. The `&'static str` is the literal token.
    SpecialTokenBlock(&'static str),
}

/// Where the model puts its chain-of-thought in the response.
///
/// Sub-quirks for `SeparateField` and `InlineThinkTags` (handled by the
/// extractor pipeline, not by this enum):
/// - When `content` and `tool_calls` are both empty but reasoning text is
///   non-empty, promote the reasoning into `content` so the executor can
///   render *something* instead of falling back to "I don't have enough
///   information".
/// - For `InlineThinkTags`, strip a single leading `<think>...</think>` from
///   content. Don't try to be clever with nested or partial tags — emit
///   as-is when malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSurface {
    /// No reasoning emitted (Llama base, GPT-class non-reasoning).
    None,
    /// Top-level `reasoning_content` field on the response (DeepSeek-R1,
    /// Kimi K2 thinking).
    SeparateField,
    /// `<think>...</think>` blocks inline in `content` (Qwen, Gemma 4,
    /// Magistral).
    InlineThinkTags,
    /// Native typed content blocks — Anthropic `thinking` blocks, Gemini
    /// `part.thought: true`. Read straight from the wire shape; never
    /// surfaces as text in `content`.
    NativeContentBlock,
    /// OpenAI o-series: reasoning is hidden server-side; only token counts
    /// are visible. Nothing to extract.
    HiddenServerSide,
}

/// What the chat template requires of message order. Only matters for local
/// inference (llama.cpp / vLLM / SGLang); cloud APIs handle this server-side.
///
/// `SystemMustBeFirst` is the constraint that drove
/// `AgentBuilder::external_system_suffix` — we never push mid-stream
/// `Role::System` for any host (memory, attachments, compaction). The
/// content folds into the leading system message via the suffix instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateStrictness {
    /// Cloud APIs, Kimi GGUF, Llama 3.2/4 templates — anything goes.
    Lenient,
    /// Qwen3.5/3.6, Gemma 4, DeepSeek V3.1: any system message past
    /// position 0 raises `'System message must be at the beginning'`.
    SystemMustBeFirst,
    /// Mistral `[INST]` — system content is wrapped into the first user
    /// message. We pre-fold it ourselves to avoid template surprises.
    SystemAbsorbedIntoUser,
}

/// Pre-parse fixes applied to tool-call argument strings before they hit
/// `serde_json`. Composable: callers run every enabled flag in order.
///
/// Implemented as a struct of bools rather than the `bitflags` crate to
/// keep the dependency surface minimal — adding a flag is just adding a
/// `pub bool` field plus a row in the match in `seed.rs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ToolArgRepair {
    /// DeepSeek streaming concatenates raw control chars (0x00–0x1F)
    /// into argument deltas; convert each to its `\uXXXX` escape before
    /// JSON parse. Defense-in-depth: the per-tool `do_*` handlers also
    /// repair, but doing it once at the wire boundary catches everything.
    pub control_chars_to_unicode_escape: bool,
    /// Gemma 4 via Ollama returns JSON arrays as escaped *strings* like
    /// `"[\"a\", \"b\"]"`; unescape one level before parsing.
    pub unescape_double_encoded_json_arrays: bool,
}

impl ToolArgRepair {
    /// No repair applied — the OpenAI/Anthropic baseline.
    pub const fn empty() -> Self {
        Self {
            control_chars_to_unicode_escape: false,
            unescape_double_encoded_json_arrays: false,
        }
    }

    /// True iff at least one repair flag is set.
    pub fn any(&self) -> bool {
        self.control_chars_to_unicode_escape || self.unescape_double_encoded_json_arrays
    }
}

/// The full per-model quirks profile. One value per `ModelFamily` (see
/// `seed::quirks_for_family`).
///
/// `Default` is the safety net: trust structured fields, no reasoning
/// extraction, no template constraints, no repair. Every model the user has
/// not profiled falls through to this — reproducing today's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelQuirks {
    pub tool_extraction: ToolExtractionStrategy,
    pub reasoning_surface: ReasoningSurface,
    pub template_strictness: TemplateStrictness,
    pub tool_arg_repair: ToolArgRepair,
    /// DeepSeek-R1: prior turn's `reasoning_content` must be echoed back
    /// when a tool call fired. Sending it when not required → 400; omitting
    /// it when required → state corruption.
    pub echo_reasoning_on_tool_turn: bool,
    /// Gemma 4 requires *some* system or developer message in position 0.
    /// We always emit one (the persona header), but if a future caller
    /// drops it for some flow, this flag asks the provider to inject a
    /// minimal placeholder rather than 500.
    pub system_message_required: bool,
}

impl Default for ModelQuirks {
    fn default() -> Self {
        Self {
            tool_extraction: ToolExtractionStrategy::Structured,
            reasoning_surface: ReasoningSurface::None,
            template_strictness: TemplateStrictness::Lenient,
            tool_arg_repair: ToolArgRepair::empty(),
            echo_reasoning_on_tool_turn: false,
            system_message_required: false,
        }
    }
}

/// Apply per-model quirks to a freshly parsed `LlmResponse`. Today (slice 2)
/// every branch is a no-op for the default `ModelQuirks` — i.e. for every
/// `ModelFamily` whose user has not explicitly opted into a non-Structured
/// extraction strategy. Slice 3 fills in the inline-XML extractor, the
/// reasoning-content promotion, and the tool-arg repair pipelines.
///
/// Provider response handlers call this exactly once on each completed
/// `LlmResponse` (and once per assembled streaming response). Keep the
/// branches keyed by the discriminants in `ModelQuirks` — never branch on
/// `provider_id` or `model_used` here.
pub fn apply_to_response(quirks: &ModelQuirks, response: &mut LlmResponse) {
    // --- Tool call extraction --------------------------------------
    // Non-Structured strategies recover tool calls from `content` when the
    // response's `tool_calls` field is empty, then strip them from the prose.
    if response.tool_calls.is_empty() {
        match quirks.tool_extraction {
            ToolExtractionStrategy::Structured => {}
            ToolExtractionStrategy::InlineXmlQwenStyle => {
                let (stripped, calls) = extractors::extract_qwen_style(&response.content);
                if !calls.is_empty() {
                    response.content = stripped;
                    response.tool_calls = calls;
                    // The provider already set finish_reason to Stop; if we
                    // recovered tool calls, surface ToolUse so the executor
                    // dispatches them instead of treating the turn as final.
                    response.finish_reason = athen_core::llm::FinishReason::ToolUse;
                }
            }
            // Other inline strategies land in later slices.
            ToolExtractionStrategy::InlineXmlVendorTagged(_)
            | ToolExtractionStrategy::InlineJsonLlama
            | ToolExtractionStrategy::InlinePythonicLlama
            | ToolExtractionStrategy::SpecialTokenBlock(_) => {}
        }
    }

    // --- Reasoning surface handling --------------------------------
    // For inline-think-tag models, strip a single leading think block from
    // content regardless of whether anything is empty — keeps the user view
    // clean. (Reasoning-content stays accessible via response.reasoning_content
    // for tools that want it.)
    if matches!(quirks.reasoning_surface, ReasoningSurface::InlineThinkTags) {
        response.content = extractors::strip_leading_think_tag(&response.content);
    }

    // For separate-field models (DeepSeek-R1, Kimi K2 thinking), promote
    // reasoning into content when both content and tool_calls are empty —
    // otherwise the executor falls back to the hardcoded "I don't have
    // enough information" placeholder.
    if response.content.is_empty()
        && response.tool_calls.is_empty()
        && matches!(quirks.reasoning_surface, ReasoningSurface::SeparateField)
    {
        if let Some(reasoning) = response.reasoning_content.as_ref() {
            if !reasoning.trim().is_empty() {
                // The reasoning sometimes carries its own leading think
                // tag (especially when proxied through llama.cpp); strip it
                // for the same UX reason as the inline path.
                response.content = extractors::strip_leading_think_tag(reasoning);
            }
        }
    }

    // --- Tool arg repair -------------------------------------------
    if quirks.tool_arg_repair.any() && !response.tool_calls.is_empty() {
        for call in response.tool_calls.iter_mut() {
            repair::apply(&quirks.tool_arg_repair, &mut call.arguments);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::llm::{FinishReason, TokenUsage};

    fn empty_response() -> LlmResponse {
        LlmResponse {
            content: String::new(),
            reasoning_content: None,
            model_used: "test".into(),
            provider: "test".into(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                estimated_cost_usd: None,
            },
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
        }
    }

    #[test]
    fn default_quirks_reproduce_baseline_behavior() {
        let q = ModelQuirks::default();
        assert_eq!(q.tool_extraction, ToolExtractionStrategy::Structured);
        assert_eq!(q.reasoning_surface, ReasoningSurface::None);
        assert_eq!(q.template_strictness, TemplateStrictness::Lenient);
        assert!(!q.tool_arg_repair.any());
        assert!(!q.echo_reasoning_on_tool_turn);
        assert!(!q.system_message_required);
    }

    #[test]
    fn apply_to_response_default_is_byte_identical_no_op() {
        let mut resp = empty_response();
        resp.content = "Hello.".into();
        let before = resp.clone();
        apply_to_response(&ModelQuirks::default(), &mut resp);
        assert_eq!(resp.content, before.content);
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn apply_to_response_qwen_extracts_inline_tool_call_and_flips_finish_reason() {
        let qwen_quirks = seed::quirks_for_family(athen_core::llm::ModelFamily::Qwen35Local);
        let mut resp = empty_response();
        resp.content = "Reading file.\n<tool_call><function=read_file><parameter=path>src/main.rs</parameter></function></tool_call>".into();
        apply_to_response(&qwen_quirks, &mut resp);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "read_file");
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        assert_eq!(resp.content, "Reading file.");
    }

    #[test]
    fn apply_to_response_qwen_strips_leading_think_block() {
        let qwen_quirks = seed::quirks_for_family(athen_core::llm::ModelFamily::Qwen35Local);
        let mut resp = empty_response();
        resp.content = "<think>let me think</think>\nThe answer is here.".into();
        apply_to_response(&qwen_quirks, &mut resp);
        assert_eq!(resp.content, "The answer is here.");
    }

    #[test]
    fn apply_to_response_deepseek_r1_promotes_reasoning_when_content_empty() {
        let r1_quirks = seed::quirks_for_family(athen_core::llm::ModelFamily::DeepSeekR1);
        let mut resp = empty_response();
        resp.content = String::new();
        resp.reasoning_content = Some("Here is my answer.".into());
        apply_to_response(&r1_quirks, &mut resp);
        assert_eq!(resp.content, "Here is my answer.");
    }

    #[test]
    fn apply_to_response_deepseek_r1_does_not_overwrite_real_content() {
        let r1_quirks = seed::quirks_for_family(athen_core::llm::ModelFamily::DeepSeekR1);
        let mut resp = empty_response();
        resp.content = "Real answer.".into();
        resp.reasoning_content = Some("Background reasoning.".into());
        apply_to_response(&r1_quirks, &mut resp);
        assert_eq!(resp.content, "Real answer.");
    }

    #[test]
    fn apply_to_response_runs_arg_repair_for_deepseek_v4() {
        let v4_quirks = seed::quirks_for_family(athen_core::llm::ModelFamily::DeepSeekV4Chat);
        let mut resp = empty_response();
        resp.tool_calls = vec![athen_core::llm::ToolCall {
            id: "x".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"cmd": "ls\u{0001}-la"}),
        }];
        apply_to_response(&v4_quirks, &mut resp);
        assert_eq!(resp.tool_calls[0].arguments["cmd"], "ls -la");
    }

    #[test]
    fn tool_arg_repair_any_tracks_individual_flags() {
        assert!(!ToolArgRepair::empty().any());
        assert!(ToolArgRepair {
            control_chars_to_unicode_escape: true,
            ..ToolArgRepair::empty()
        }
        .any());
        assert!(ToolArgRepair {
            unescape_double_encoded_json_arrays: true,
            ..ToolArgRepair::empty()
        }
        .any());
    }
}
