use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::Stream;

use crate::error::Result;
use crate::tool::ToolDefinition;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ModelProfile {
    Powerful,
    Fast,
    Code,
    Cheap,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub profile: ModelProfile,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub system_prompt: Option<String>,
    /// Cross-provider "think harder / think less" knob. `Default` (the
    /// default) omits any reasoning field from the wire request so each
    /// provider applies its built-in default — opt-in behaviour, see
    /// `docs/REASONING_EFFORT.md` for the per-provider mapping table.
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

/// Unifying knob for provider-specific reasoning controls. Each adapter
/// maps these variants to its wire shape (OpenAI `reasoning.effort`,
/// Anthropic `thinking.budget_tokens`, Gemini `thinkingLevel` /
/// `thinkingBudget`, DeepSeek `reasoning_effort`, local
/// `chat_template_kwargs.enable_thinking` or Ollama `think`). See
/// `docs/REASONING_EFFORT.md` §"Per-provider mapping table" for the full
/// spec. `Default` and `Off` are distinct: `Default` omits the field
/// entirely so the provider applies its built-in default, `Off`
/// explicitly disables reasoning where the provider supports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Default,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    /// Stable wire identifier — round-trips through arc-meta storage and
    /// the `delegate_to_agent` tool param.
    pub fn to_wire_str(self) -> &'static str {
        match self {
            ReasoningEffort::Default => "default",
            ReasoningEffort::Off => "off",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "default" => Ok(ReasoningEffort::Default),
            "off" | "none" | "disabled" => Ok(ReasoningEffort::Off),
            "minimal" => Ok(ReasoningEffort::Minimal),
            "low" => Ok(ReasoningEffort::Low),
            "medium" | "med" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            "max" | "xhigh" => Ok(ReasoningEffort::Max),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    /// Text accompanied by one or more inline images. Each provider adapter
    /// is responsible for serialising this into its native multimodal wire
    /// format (Anthropic content blocks, OpenAI image_url parts, Gemini
    /// inlineData parts, etc). Providers without vision support must reject
    /// this variant rather than silently dropping the images.
    Multimodal {
        text: String,
        images: Vec<ImageInput>,
    },
    /// Pre-shaped, provider-specific JSON. Used for tool result blocks and
    /// other cases where the wire representation is already finalised.
    Structured(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInput {
    /// IANA media type, e.g. `image/png`, `image/jpeg`, `image/webp`.
    pub mime_type: String,
    pub data: ImageData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageData {
    /// Raw bytes encoded as base64 (no data-URL prefix).
    Base64 { data: String },
    /// Public URL the provider can fetch directly.
    Url { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub model_used: String,
    pub provider: String,
    pub usage: TokenUsage,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Opaque, provider-specific signature returned alongside the tool
    /// call that we must echo back unchanged when replaying this turn in
    /// history. Today only Gemini 3 / thinking-mode 2.5 populate this
    /// (the `thoughtSignature` field on `functionCall` parts) — passing
    /// it back is mandatory or the API rejects the next turn with HTTP
    /// 400. Other providers leave this `None` and the field is omitted
    /// from the wire JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub estimated_cost_usd: Option<f64>,
    /// Cache hits — the portion of `prompt_tokens` that came from a prompt
    /// cache. Surfaces DeepSeek `prompt_cache_hit_tokens`, OpenAI
    /// `prompt_tokens_details.cached_tokens`, Anthropic
    /// `cache_read_input_tokens`, and Gemini
    /// `usageMetadata.cachedContentTokenCount`. `None` when the provider
    /// didn't report it (older response shapes, providers without caching).
    /// Cost estimators should subtract this from `prompt_tokens` before
    /// applying the full input rate and bill the cached portion at the
    /// provider's discounted rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Tokens written into a fresh cache entry. Anthropic-only today
    /// (`cache_creation_input_tokens`); billed at 1.25× (5-min TTL) or
    /// 2× (1-h TTL) the base input rate. `None` for every other provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolUse,
    MaxTokens,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmChunk {
    pub delta: String,
    pub is_final: bool,
    pub is_thinking: bool,
    /// Tool calls extracted from streaming SSE chunks.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Token usage for the whole streamed turn, populated only on the
    /// terminal usage-bearing chunk (the provider emits a single chunk
    /// carrying `Some(usage)` when the wire delivers final usage —
    /// OpenAI/DeepSeek's usage-only SSE event, Anthropic's combined
    /// `message_start` + `message_delta`, Gemini's final `usageMetadata`).
    /// Every other chunk leaves this `None`. The executor collects the
    /// last `Some(usage)` to build the synthetic `LlmResponse`, and the
    /// router's stream wrapper records it against the budget on clean
    /// completion. Omitted from the wire when `None` so existing chunk
    /// JSON stays byte-identical and old payloads still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub daily_limit_usd: Option<f64>,
    pub spent_today_usd: f64,
    pub remaining_usd: Option<f64>,
    pub tokens_used_today: u64,
}

/// Type alias for LLM streaming response
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmChunk>> + Send>>;

/// User-selected model family. Drives the per-model quirks lookup
/// (`athen-llm::quirks`) and the default slug pre-fill in provider config UI.
///
/// We do **not** auto-detect family from the slug: the user picks the family
/// in the dropdown and edits the slug independently, so a same-format model
/// version drop ships same-day without a code change. See
/// `docs/PER_MODEL_QUIRKS.md` §4.
///
/// `Default` is the safety net: any provider config whose user has not
/// explicitly selected a family falls through to `Default`, which the quirks
/// table maps to "trust structured fields, no extraction, no repair" —
/// reproducing today's behavior for every unprofiled model.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ModelFamily {
    /// Catch-all preset for any model the user hasn't profiled.
    #[default]
    Default,
    ClaudeOpus47,
    ClaudeSonnet46,
    ClaudeHaiku45,
    Gpt5,
    OpenAiO3,
    Gemini3Pro,
    Gemini3Flash,
    DeepSeekV4Chat,
    DeepSeekR1,
    Qwen35Local,
    Qwen36Local,
    Gemma4Local,
    KimiK26Cloud,
    /// Kimi K2.7 Code (Moonshot, June 2026). Coding-agent model with forced
    /// thinking (`reasoning_content` separate field) that must be echoed
    /// back on tool turns (`preserve_thinking`).
    KimiK27Code,
    MiniMaxM25Cloud,
    MiniMaxM27Cloud,
    Llama32Instruct,
    Llama33Instruct,
    Llama4Instruct,
    MistralLarge3,
    MagistralMedium,
    Codestral2508,
    /// DeepSeek V4 Pro (flagship MoE, 2026). Structured + control-char
    /// repair. Add `with_family(DeepSeekR1)` if pointing at the thinking-mode
    /// variant of the same endpoint.
    DeepSeekV4Pro,
    /// Qwen3-Coder Next (Feb 2026). Inline XML tool calls with a different
    /// shape than Qwen3.5/3.6: `<TOOL_NAME><parameter=KEY>VAL</parameter></TOOL_NAME>`
    /// instead of `<tool_call><function=NAME>...`.
    Qwen3CoderNext,
    /// xAI Grok 4 (Grok 4.3 lineage). 1M context, OpenAI-compat structured
    /// tool calls, lenient template.
    Grok4,
    /// Liquid AI LFM2.5-8B-A1B (on-device MoE, May 2026). Reasoning-only
    /// (`<think>` blocks) with *pythonic* tool calls delimited by
    /// `<|tool_call_start|>[func(arg='v', flag=True)]<|tool_call_end|>`.
    /// vLLM and recent llama.cpp parse the calls server-side (Structured
    /// path); older llama.cpp surfaces them as text, so we extract inline.
    /// No reasoning echo-back required (unlike DeepSeek-R1). Native system
    /// role; recommended sampling temp 0.2.
    Lfm25Local,
}

impl ModelFamily {
    /// Human-readable label for UI dropdowns.
    pub fn display_label(self) -> &'static str {
        match self {
            ModelFamily::Default => "Default (unknown / generic)",
            ModelFamily::ClaudeOpus47 => "Claude Opus 4.7",
            ModelFamily::ClaudeSonnet46 => "Claude Sonnet 4.6",
            ModelFamily::ClaudeHaiku45 => "Claude Haiku 4.5",
            ModelFamily::Gpt5 => "GPT-5",
            ModelFamily::OpenAiO3 => "OpenAI o3",
            ModelFamily::Gemini3Pro => "Gemini 3 Pro",
            ModelFamily::Gemini3Flash => "Gemini 3 Flash",
            ModelFamily::DeepSeekV4Chat => "DeepSeek-V4 chat",
            ModelFamily::DeepSeekR1 => "DeepSeek-R1 reasoner",
            ModelFamily::Qwen35Local => "Qwen 3.5 (local)",
            ModelFamily::Qwen36Local => "Qwen 3.6 (local)",
            ModelFamily::Gemma4Local => "Gemma 4 (local)",
            ModelFamily::KimiK26Cloud => "Kimi K2.6 cloud",
            ModelFamily::KimiK27Code => "Kimi K2.7 Code cloud",
            ModelFamily::MiniMaxM25Cloud => "MiniMax M2.5 cloud",
            ModelFamily::MiniMaxM27Cloud => "MiniMax M2.7 cloud",
            ModelFamily::Llama32Instruct => "Llama 3.2 instruct (Vision / 70B class)",
            ModelFamily::Llama33Instruct => "Llama 3.3 70B instruct",
            ModelFamily::Llama4Instruct => "Llama 4 (Scout / Maverick)",
            ModelFamily::MistralLarge3 => "Mistral Large 3",
            ModelFamily::MagistralMedium => "Magistral Medium",
            ModelFamily::Codestral2508 => "Codestral 25.08",
            ModelFamily::DeepSeekV4Pro => "DeepSeek V4 Pro",
            ModelFamily::Qwen3CoderNext => "Qwen3-Coder Next",
            ModelFamily::Grok4 => "xAI Grok 4",
            ModelFamily::Lfm25Local => "Liquid LFM2.5-8B-A1B (local)",
        }
    }

    /// Stable wire identifier — round-trips through configs, the
    /// settings IPC layer, and the headless CLI's `--family` flag.
    /// Matches the enum variant name verbatim.
    pub fn wire_id(self) -> &'static str {
        match self {
            ModelFamily::Default => "Default",
            ModelFamily::ClaudeOpus47 => "ClaudeOpus47",
            ModelFamily::ClaudeSonnet46 => "ClaudeSonnet46",
            ModelFamily::ClaudeHaiku45 => "ClaudeHaiku45",
            ModelFamily::Gpt5 => "Gpt5",
            ModelFamily::OpenAiO3 => "OpenAiO3",
            ModelFamily::Gemini3Pro => "Gemini3Pro",
            ModelFamily::Gemini3Flash => "Gemini3Flash",
            ModelFamily::DeepSeekV4Chat => "DeepSeekV4Chat",
            ModelFamily::DeepSeekR1 => "DeepSeekR1",
            ModelFamily::Qwen35Local => "Qwen35Local",
            ModelFamily::Qwen36Local => "Qwen36Local",
            ModelFamily::Gemma4Local => "Gemma4Local",
            ModelFamily::KimiK26Cloud => "KimiK26Cloud",
            ModelFamily::KimiK27Code => "KimiK27Code",
            ModelFamily::MiniMaxM25Cloud => "MiniMaxM25Cloud",
            ModelFamily::MiniMaxM27Cloud => "MiniMaxM27Cloud",
            ModelFamily::Llama32Instruct => "Llama32Instruct",
            ModelFamily::Llama33Instruct => "Llama33Instruct",
            ModelFamily::Llama4Instruct => "Llama4Instruct",
            ModelFamily::MistralLarge3 => "MistralLarge3",
            ModelFamily::MagistralMedium => "MagistralMedium",
            ModelFamily::Codestral2508 => "Codestral2508",
            ModelFamily::DeepSeekV4Pro => "DeepSeekV4Pro",
            ModelFamily::Qwen3CoderNext => "Qwen3CoderNext",
            ModelFamily::Grok4 => "Grok4",
            ModelFamily::Lfm25Local => "Lfm25Local",
        }
    }

    /// Inverse of `wire_id`. Returns `None` for unknown identifiers so
    /// the caller can decide whether to fall back to `Default` or hard-error.
    pub fn from_wire_id(s: &str) -> Option<Self> {
        Self::all().iter().copied().find(|f| f.wire_id() == s)
    }

    /// Every variant, in display order. Used by the provider-config UI to
    /// populate the family dropdown.
    pub fn all() -> &'static [ModelFamily] {
        &[
            ModelFamily::Default,
            ModelFamily::ClaudeOpus47,
            ModelFamily::ClaudeSonnet46,
            ModelFamily::ClaudeHaiku45,
            ModelFamily::Gpt5,
            ModelFamily::OpenAiO3,
            ModelFamily::Gemini3Pro,
            ModelFamily::Gemini3Flash,
            ModelFamily::DeepSeekV4Chat,
            ModelFamily::DeepSeekR1,
            ModelFamily::Qwen35Local,
            ModelFamily::Qwen36Local,
            ModelFamily::Gemma4Local,
            ModelFamily::KimiK26Cloud,
            ModelFamily::KimiK27Code,
            ModelFamily::MiniMaxM25Cloud,
            ModelFamily::MiniMaxM27Cloud,
            ModelFamily::Llama32Instruct,
            ModelFamily::Llama33Instruct,
            ModelFamily::Llama4Instruct,
            ModelFamily::MistralLarge3,
            ModelFamily::MagistralMedium,
            ModelFamily::Codestral2508,
            ModelFamily::DeepSeekV4Pro,
            ModelFamily::Qwen3CoderNext,
            ModelFamily::Grok4,
            ModelFamily::Lfm25Local,
        ]
    }
}

#[cfg(test)]
mod model_family_tests {
    use super::*;

    #[test]
    fn default_is_the_fallback_variant() {
        assert_eq!(ModelFamily::default(), ModelFamily::Default);
    }

    #[test]
    fn every_variant_has_a_label_and_appears_in_all() {
        let listed = ModelFamily::all();
        for f in listed {
            assert!(!f.display_label().is_empty());
        }
        // round-trip: every variant produced by `all()` should be unique
        let mut seen = std::collections::HashSet::new();
        for f in listed {
            assert!(seen.insert(*f), "duplicate family in all(): {:?}", f);
        }
    }

    #[test]
    fn wire_id_round_trips_for_every_variant() {
        for f in ModelFamily::all() {
            let id = f.wire_id();
            assert!(!id.is_empty(), "empty wire_id for {:?}", f);
            assert_eq!(
                ModelFamily::from_wire_id(id),
                Some(*f),
                "round-trip failed for {:?} (wire_id={id})",
                f
            );
        }
    }

    #[test]
    fn from_wire_id_rejects_unknown() {
        assert_eq!(ModelFamily::from_wire_id(""), None);
        assert_eq!(ModelFamily::from_wire_id("nope"), None);
        // case-sensitive — wire ids match enum variant names exactly
        assert_eq!(ModelFamily::from_wire_id("default"), None);
    }
}

#[cfg(test)]
mod llm_chunk_tests {
    use super::*;

    #[test]
    fn chunk_without_usage_round_trips_and_omits_field() {
        let chunk = LlmChunk {
            delta: "hi".into(),
            is_final: false,
            is_thinking: false,
            tool_calls: vec![],
            usage: None,
        };
        let json = serde_json::to_value(&chunk).unwrap();
        // `usage: None` is skipped on the wire, keeping old payloads identical.
        assert!(json.get("usage").is_none(), "usage must be omitted: {json}");
        let back: LlmChunk = serde_json::from_value(json).unwrap();
        assert!(back.usage.is_none());
        assert_eq!(back.delta, "hi");
    }

    #[test]
    fn chunk_with_usage_round_trips() {
        let chunk = LlmChunk {
            delta: String::new(),
            is_final: true,
            is_thinking: false,
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 40,
                total_tokens: 140,
                estimated_cost_usd: Some(0.0021),
                cached_tokens: Some(64),
                cache_creation_tokens: None,
            }),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let back: LlmChunk = serde_json::from_str(&json).unwrap();
        let u = back.usage.expect("usage present");
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 40);
        assert_eq!(u.total_tokens, 140);
        assert_eq!(u.cached_tokens, Some(64));
        assert!(back.is_final);
    }

    #[test]
    fn old_chunk_json_without_usage_still_deserializes() {
        // A chunk payload from before the `usage` field existed.
        let legacy = r#"{"delta":"x","is_final":true,"is_thinking":false,"tool_calls":[]}"#;
        let back: LlmChunk = serde_json::from_str(legacy).unwrap();
        assert!(back.usage.is_none());
        assert_eq!(back.delta, "x");
    }
}

#[cfg(test)]
mod reasoning_effort_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn default_is_the_omit_variant() {
        assert_eq!(ReasoningEffort::default(), ReasoningEffort::Default);
    }

    #[test]
    fn wire_str_round_trips_for_every_variant() {
        for v in [
            ReasoningEffort::Default,
            ReasoningEffort::Off,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            assert_eq!(ReasoningEffort::from_str(v.to_wire_str()).unwrap(), v);
        }
    }

    #[test]
    fn from_str_accepts_synonyms_and_blank() {
        assert_eq!(
            ReasoningEffort::from_str("").unwrap(),
            ReasoningEffort::Default
        );
        assert_eq!(
            ReasoningEffort::from_str("None").unwrap(),
            ReasoningEffort::Off
        );
        assert_eq!(
            ReasoningEffort::from_str(" MED ").unwrap(),
            ReasoningEffort::Medium
        );
        assert_eq!(
            ReasoningEffort::from_str("xhigh").unwrap(),
            ReasoningEffort::Max
        );
        assert!(ReasoningEffort::from_str("bogus").is_err());
    }

    #[test]
    fn serde_uses_lowercase_wire_names() {
        let json = serde_json::to_string(&ReasoningEffort::Medium).unwrap();
        assert_eq!(json, "\"medium\"");
        let back: ReasoningEffort = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(back, ReasoningEffort::High);
    }

    #[test]
    fn llm_request_defaults_serialize_to_default_variant() {
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["reasoning_effort"], "default");
    }
}
