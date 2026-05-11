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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolUse,
    MaxTokens,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChunk {
    pub delta: String,
    pub is_final: bool,
    pub is_thinking: bool,
    /// Tool calls extracted from streaming SSE chunks.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
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
    MiniMaxM25Cloud,
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
            ModelFamily::MiniMaxM25Cloud => "MiniMax M2.5 cloud",
            ModelFamily::Llama32Instruct => "Llama 3.2 instruct (Vision / 70B class)",
            ModelFamily::Llama33Instruct => "Llama 3.3 70B instruct",
            ModelFamily::Llama4Instruct => "Llama 4 (Scout / Maverick)",
            ModelFamily::MistralLarge3 => "Mistral Large 3",
            ModelFamily::MagistralMedium => "Magistral Medium",
            ModelFamily::Codestral2508 => "Codestral 25.08",
            ModelFamily::DeepSeekV4Pro => "DeepSeek V4 Pro",
            ModelFamily::Qwen3CoderNext => "Qwen3-Coder Next",
            ModelFamily::Grok4 => "xAI Grok 4",
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
            ModelFamily::MiniMaxM25Cloud => "MiniMaxM25Cloud",
            ModelFamily::Llama32Instruct => "Llama32Instruct",
            ModelFamily::Llama33Instruct => "Llama33Instruct",
            ModelFamily::Llama4Instruct => "Llama4Instruct",
            ModelFamily::MistralLarge3 => "MistralLarge3",
            ModelFamily::MagistralMedium => "MagistralMedium",
            ModelFamily::Codestral2508 => "Codestral2508",
            ModelFamily::DeepSeekV4Pro => "DeepSeekV4Pro",
            ModelFamily::Qwen3CoderNext => "Qwen3CoderNext",
            ModelFamily::Grok4 => "Grok4",
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
            ModelFamily::MiniMaxM25Cloud,
            ModelFamily::Llama32Instruct,
            ModelFamily::Llama33Instruct,
            ModelFamily::Llama4Instruct,
            ModelFamily::MistralLarge3,
            ModelFamily::MagistralMedium,
            ModelFamily::Codestral2508,
            ModelFamily::DeepSeekV4Pro,
            ModelFamily::Qwen3CoderNext,
            ModelFamily::Grok4,
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
