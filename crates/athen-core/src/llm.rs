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
