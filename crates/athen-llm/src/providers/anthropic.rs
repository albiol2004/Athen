//! Anthropic (Claude) provider adapter.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

use crate::quirks::{self, seed, ModelQuirks};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic (Claude) LLM provider.
pub struct AnthropicProvider {
    api_key: String,
    default_model: String,
    client: Client,
    base_url: String,
    supports_vision: bool,
    supports_documents: bool,
    /// Quirks profile. Anthropic's wire format already exposes thinking as
    /// native typed content blocks, so the `apply_to_response` pipeline is
    /// a no-op for Claude families — kept for consistency with the other
    /// providers.
    quirks: ModelQuirks,
    /// Family selection drives reasoning-effort mapping: Opus 4.7 takes
    /// `{ type: "adaptive" }` and rejects `type: "enabled"`; Haiku 4.5
    /// takes only `type: "enabled"`; Sonnet 4.6 accepts either. Default
    /// keeps reproductive byte-for-byte behaviour with the pre-feature
    /// adapter for unprofiled configs.
    family: ModelFamily,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(api_key: String, default_model: String) -> Self {
        Self {
            api_key,
            default_model,
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest Client should build with timeout"),
            base_url: DEFAULT_BASE_URL.to_string(),
            supports_vision: false,
            supports_documents: false,
            // Default quirks; callers pick a Claude family via with_family
            // for symmetry — the values don't affect Claude parsing today.
            quirks: ModelQuirks::default(),
            family: ModelFamily::Default,
        }
    }

    /// Set the model family. Cosmetic for Anthropic (Claude exposes
    /// thinking as native typed blocks, no inline extraction needed) but
    /// keeps the construction surface uniform across providers.
    pub fn with_family(mut self, family: ModelFamily) -> Self {
        self.quirks = seed::quirks_for_family(family);
        self.family = family;
        self
    }

    /// Mark the configured `default_model` as vision-capable. Caller is
    /// responsible for matching this to the actual model — passing images
    /// to a non-vision Claude model returns a 400 from the API.
    pub fn with_vision(mut self, supported: bool) -> Self {
        self.supports_vision = supported;
        self
    }

    /// Mark the configured `default_model` as document-capable (native
    /// PDF input via Anthropic document content blocks). Claude 3.5+.
    pub fn with_documents(mut self, supported: bool) -> Self {
        self.supports_documents = supported;
        self
    }

    /// Override the base URL (useful for testing or proxies).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    /// Override the HTTP client.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Build the Anthropic API request body from our generic request.
    fn build_request_body(&self, request: &LlmRequest) -> AnthropicRequest {
        let messages: Vec<AnthropicMessage> = request
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                let is_tool = m.role == Role::Tool;
                let is_assistant = m.role == Role::Assistant;
                let content = match &m.content {
                    MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                    MessageContent::Structured(v) => {
                        structured_to_anthropic_content(v, is_assistant, is_tool)
                    }
                    MessageContent::Multimodal { text, images } => {
                        anthropic_multimodal_blocks(text, images)
                    }
                };
                AnthropicMessage {
                    role: match m.role {
                        Role::User | Role::Tool => "user".to_string(),
                        Role::Assistant => "assistant".to_string(),
                        Role::System => "user".to_string(), // filtered above
                    },
                    content,
                }
            })
            .collect();

        AnthropicRequest {
            model: self.default_model.clone(),
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            system: request.system_prompt.clone(),
            stream: false,
            thinking: map_reasoning_effort(self.family, request.reasoning_effort),
        }
    }

    /// Map Anthropic API errors to AthenError.
    fn map_error(&self, status: reqwest::StatusCode, body: &str) -> AthenError {
        let message = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            format!("rate limited: {}", body)
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            format!("authentication failed: {}", body)
        } else if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            format!("server overloaded ({}): {}", status, body)
        } else {
            format!("HTTP {}: {}", status, body)
        };

        AthenError::LlmProvider {
            provider: "anthropic".into(),
            message,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request_body(request);
        let url = format!("{}/v1/messages", self.base_url);

        debug!(model = %body.model, "sending Anthropic completion request");

        let http_response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AthenError::Timeout(Duration::from_secs(120))
                } else {
                    AthenError::LlmProvider {
                        provider: "anthropic".into(),
                        message: format!("request failed: {}", e),
                    }
                }
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let api_response: AnthropicResponse =
            http_response
                .json()
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: "anthropic".into(),
                    message: format!("failed to parse response: {}", e),
                })?;

        // Extract text content from response blocks.
        let content = api_response
            .content
            .iter()
            .filter_map(|block| {
                if block.content_type == "text" {
                    block.text.clone()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");

        // Extract tool calls if present.
        let tool_calls: Vec<ToolCall> = api_response
            .content
            .iter()
            .filter(|block| block.content_type == "tool_use")
            .map(|block| ToolCall {
                id: block.id.clone().unwrap_or_default(),
                name: block.name.clone().unwrap_or_default(),
                arguments: block.input.clone().unwrap_or(serde_json::Value::Null),
                thought_signature: None,
            })
            .collect();

        let finish_reason = match api_response.stop_reason.as_deref() {
            Some("end_turn") | Some("stop") => FinishReason::Stop,
            Some("tool_use") => FinishReason::ToolUse,
            Some("max_tokens") => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };

        let usage = TokenUsage {
            prompt_tokens: api_response.usage.input_tokens,
            completion_tokens: api_response.usage.output_tokens,
            total_tokens: api_response.usage.input_tokens + api_response.usage.output_tokens,
            estimated_cost_usd: Some(estimate_anthropic_cost(
                &api_response.model,
                api_response.usage.input_tokens,
                api_response.usage.output_tokens,
            )),
            ..TokenUsage::default()
        };

        let mut response = LlmResponse {
            content,
            reasoning_content: None,
            model_used: api_response.model,
            provider: "anthropic".into(),
            usage,
            tool_calls,
            finish_reason,
        };
        quirks::apply_to_response(&self.quirks, &mut response);
        Ok(response)
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let mut body = self.build_request_body(request);
        body.stream = true;
        let url = format!("{}/v1/messages", self.base_url);

        debug!(model = %body.model, "sending Anthropic streaming request");

        let http_response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AthenError::Timeout(Duration::from_secs(120))
                } else {
                    AthenError::LlmProvider {
                        provider: "anthropic".into(),
                        message: format!("streaming request failed: {}", e),
                    }
                }
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let byte_stream = http_response.bytes_stream();

        // Buffer raw bytes across TCP chunks and split on `\n\n` (the SSE
        // event boundary). Without this, an event split across two TCP
        // segments yields two partial JSON fragments that both fail to parse,
        // silently dropping the text delta — causing intermittent tool-call
        // extraction failures for inline-format models (MiniMax M2.7).
        let raw_chunks = byte_stream
            .scan(Vec::<u8>::new(), |buffer, result| {
                let emitted: Vec<Result<LlmChunk>> = match result {
                    Ok(bytes) => {
                        buffer.extend_from_slice(&bytes);
                        drain_complete_sse_events(buffer)
                    }
                    Err(e) => vec![Err(AthenError::LlmProvider {
                        provider: "anthropic".into(),
                        message: format!("stream error: {}", e),
                    })],
                };
                futures::future::ready(Some(emitted))
            })
            .flat_map(futures::stream::iter);

        // Wrap the raw SSE stream for inline tool-call extraction.
        // Models like MiniMax M2.7 emit tool calls as text in
        // `content_block_delta` events, not as structured `tool_use`
        // blocks. Buffer content deltas and run `extract_streaming_tail`
        // at end-of-stream to recover them.
        let quirks = self.quirks;
        let chunk_stream = futures::stream::unfold(
            (
                Box::pin(raw_chunks)
                    as std::pin::Pin<Box<dyn futures::Stream<Item = Result<LlmChunk>> + Send>>,
                String::new(),
                false,
                quirks,
            ),
            |(mut inner, mut content_buf, mut saw_structured, quirks)| async move {
                let item = inner.next().await?;
                let extra = match &item {
                    Ok(chunk) => {
                        if !chunk.delta.is_empty() && !chunk.is_thinking {
                            content_buf.push_str(&chunk.delta);
                        }
                        if !chunk.tool_calls.is_empty() {
                            saw_structured = true;
                        }
                        if chunk.is_final {
                            quirks::extract_streaming_tail(&quirks, &content_buf, saw_structured)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                };
                let mut out = vec![item];
                if let Some(tail) = extra {
                    out.push(Ok(tail));
                }
                Some((
                    futures::stream::iter(out),
                    (inner, content_buf, saw_structured, quirks),
                ))
            },
        )
        .flatten();

        Ok(Box::pin(chunk_stream))
    }

    async fn is_available(&self) -> bool {
        // Simple check — just verify we have an API key configured.
        !self.api_key.is_empty()
    }

    fn supports_vision(&self) -> bool {
        self.supports_vision
    }

    fn supports_documents(&self) -> bool {
        self.supports_documents
    }
}

/// Convert the executor's internal Structured envelope into valid Anthropic
/// API content blocks. Without this, inline-extracted tool calls (MiniMax
/// M2.7, Qwen) produce garbled conversation history that the model can't
/// connect tool results to → infinite loops.
///
/// Two envelope shapes exist in the conversation:
///
/// **Assistant turn** — `{"text":"...", "tool_calls":[...], "reasoning_content":"..."}`
/// → `[{"type":"text","text":"..."},{"type":"tool_use","id":"...","name":"...","input":{...}},...]`
///
/// **Tool result** — `{"tool_call_id":"...", "content":"..."}`
/// → `[{"type":"tool_result","tool_use_id":"...","content":"..."}]`
fn structured_to_anthropic_content(
    v: &serde_json::Value,
    is_assistant: bool,
    is_tool: bool,
) -> serde_json::Value {
    if is_assistant {
        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
            let mut blocks: Vec<serde_json::Value> = Vec::new();
            if !text.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
                for call in calls {
                    let id = call.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let input = call
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    }));
                }
            }
            if blocks.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": ""}));
            }
            return serde_json::Value::Array(blocks);
        }
    }
    if is_tool {
        if let Some(tool_call_id) = v.get("tool_call_id").and_then(|i| i.as_str()) {
            let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
            return serde_json::json!([{
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": content,
            }]);
        }
    }
    // Unrecognized Structured shape — pass through as-is (backwards compat).
    v.clone()
}

/// Build the Claude `content` array for a multimodal user turn.
/// Anthropic accepts an array of content blocks; image blocks carry
/// either `source.type = "base64"` (with `media_type` + `data`) or
/// `source.type = "url"` (with `url`).
fn anthropic_multimodal_blocks(text: &str, images: &[ImageInput]) -> serde_json::Value {
    let mut blocks: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for img in images {
        let source = match &img.data {
            ImageData::Base64 { data } => serde_json::json!({
                "type": "base64",
                "media_type": img.mime_type,
                "data": data,
            }),
            ImageData::Url { url } => serde_json::json!({
                "type": "url",
                "url": url,
            }),
        };
        blocks.push(serde_json::json!({
            "type": "image",
            "source": source,
        }));
    }
    serde_json::Value::Array(blocks)
}

/// Drain every complete SSE event from `buffer`. Events are delimited by a
/// blank line (`\n\n`). Partial events (bytes after the last `\n\n`) stay
/// in the buffer for the next TCP chunk to complete. This prevents the
/// per-chunk parsing bug where a JSON payload split across two TCP
/// segments yields two fragments that both fail `serde_json::from_str`.
fn drain_complete_sse_events(buffer: &mut Vec<u8>) -> Vec<Result<LlmChunk>> {
    let mut out = Vec::new();
    while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
        let event_bytes: Vec<u8> = buffer.drain(..end).collect();
        buffer.drain(..2); // drop the `\n\n` terminator
        let event_text = String::from_utf8_lossy(&event_bytes);
        out.extend(parse_sse_chunks(&event_text));
    }
    out
}

/// Parse SSE text into LlmChunk results.
fn parse_sse_chunks(text: &str) -> Vec<Result<LlmChunk>> {
    let mut chunks = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                chunks.push(Ok(LlmChunk {
                    delta: String::new(),
                    is_final: true,
                    is_thinking: false,
                    tool_calls: vec![],
                }));
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(data) {
                Ok(event) => {
                    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    match event_type {
                        "content_block_delta" => {
                            if let Some(delta) = event
                                .get("delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str())
                            {
                                chunks.push(Ok(LlmChunk {
                                    delta: delta.to_string(),
                                    is_final: false,
                                    is_thinking: false,
                                    tool_calls: vec![],
                                }));
                            }
                        }
                        "message_stop" => {
                            chunks.push(Ok(LlmChunk {
                                delta: String::new(),
                                is_final: true,
                                is_thinking: false,
                                tool_calls: vec![],
                            }));
                        }
                        _ => {
                            // Ignore other event types (message_start, content_block_start, etc.)
                        }
                    }
                }
                Err(_) => {
                    // Skip unparseable SSE lines.
                    warn!(data = data, "failed to parse SSE event data");
                }
            }
        }
    }

    chunks
}

/// Rough cost estimation for Anthropic models (per 1M tokens pricing as
/// of 2025; this is approximate and should be kept up to date).
fn estimate_anthropic_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (0.25, 1.25)
    } else {
        // Fallback to Sonnet pricing.
        (3.0, 15.0)
    };

    let input_cost = (input_tokens as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_per_m;
    input_cost + output_cost
}

// ---------------------------------------------------------------------------
// Anthropic API types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

/// Anthropic's `thinking` knob. Sonnet/Haiku take `{ type: "enabled",
/// budget_tokens }`, Opus 4.7 takes `{ type: "adaptive" }` (no budget)
/// and rejects `type: "enabled"`. Wire shape is `{ type, ...rest }`
/// (no neighbouring discriminator field), so we hand-roll Serialize
/// rather than relying on `#[serde(tag = "type")]` which can't omit
/// the `budget_tokens` field on the adaptive variant cleanly.
#[derive(Debug)]
enum AnthropicThinking {
    Enabled { budget_tokens: u32 },
    Adaptive,
}

impl Serialize for AnthropicThinking {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            AnthropicThinking::Enabled { budget_tokens } => {
                let mut m = ser.serialize_map(Some(2))?;
                m.serialize_entry("type", "enabled")?;
                m.serialize_entry("budget_tokens", budget_tokens)?;
                m.end()
            }
            AnthropicThinking::Adaptive => {
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("type", "adaptive")?;
                m.end()
            }
        }
    }
}

/// Map our cross-provider `ReasoningEffort` to Anthropic's `thinking`
/// shape. Per `docs/REASONING_EFFORT.md`:
/// - Default / Off → omit the field (Off "best-effort" on Opus, see below)
/// - Opus 4.7 → `{ type: "adaptive" }` for Minimal..Max (the model is
///   always-thinking and rejects `type: "enabled"`)
/// - Haiku 4.5 + Sonnet 4.6 → `{ type: "enabled", budget_tokens: N }`
///   with N clamped to 64k upstream of the API
fn map_reasoning_effort(family: ModelFamily, effort: ReasoningEffort) -> Option<AnthropicThinking> {
    match (family, effort) {
        // Default and Off: omit. Opus is always-thinking so Off is
        // best-effort — sending nothing lets the model run its built-in
        // adaptive policy.
        (_, ReasoningEffort::Default | ReasoningEffort::Off) => None,
        // Opus 4.7 rejects `type: "enabled"` — only adaptive is valid.
        (ModelFamily::ClaudeOpus47, _) => Some(AnthropicThinking::Adaptive),
        // Sonnet 4.6 + Haiku 4.5 (and any other claude family) take a
        // token budget. Levels per the design doc; cap at 64k.
        (_, ReasoningEffort::Minimal) => Some(AnthropicThinking::Enabled {
            budget_tokens: 1024,
        }),
        (_, ReasoningEffort::Low) => Some(AnthropicThinking::Enabled {
            budget_tokens: 4096,
        }),
        (_, ReasoningEffort::Medium) => Some(AnthropicThinking::Enabled {
            budget_tokens: 16384,
        }),
        (_, ReasoningEffort::High) => Some(AnthropicThinking::Enabled {
            budget_tokens: 32768,
        }),
        (_, ReasoningEffort::Max) => Some(AnthropicThinking::Enabled {
            budget_tokens: 65536,
        }),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    model: String,
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multimodal_emits_text_then_image_blocks() {
        let images = vec![ImageInput {
            mime_type: "image/png".to_string(),
            data: ImageData::Base64 {
                data: "AAAA".to_string(),
            },
        }];
        let v = anthropic_multimodal_blocks("describe this", &images);
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "describe this");
        assert_eq!(arr[1]["type"], "image");
        assert_eq!(arr[1]["source"]["type"], "base64");
        assert_eq!(arr[1]["source"]["media_type"], "image/png");
        assert_eq!(arr[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn multimodal_url_source_passes_through() {
        let images = vec![ImageInput {
            mime_type: "image/jpeg".to_string(),
            data: ImageData::Url {
                url: "https://example.com/x.jpg".to_string(),
            },
        }];
        let v = anthropic_multimodal_blocks("look", &images);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[1]["source"]["type"], "url");
        assert_eq!(arr[1]["source"]["url"], "https://example.com/x.jpg");
    }

    #[test]
    fn multimodal_empty_text_omits_text_block() {
        let images = vec![ImageInput {
            mime_type: "image/png".to_string(),
            data: ImageData::Base64 {
                data: "AAAA".to_string(),
            },
        }];
        let v = anthropic_multimodal_blocks("", &images);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "image");
    }

    #[test]
    fn build_request_body_threads_multimodal_into_messages() {
        let provider =
            AnthropicProvider::new("test-key".to_string(), "claude-sonnet-4-6".to_string());
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Multimodal {
                    text: "what's this?".to_string(),
                    images: vec![ImageInput {
                        mime_type: "image/png".to_string(),
                        data: ImageData::Base64 {
                            data: "AAAA".to_string(),
                        },
                    }],
                },
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        let blocks = body.messages[0].content.as_array().expect("array content");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
    }

    /// Helper: build a minimal request and serialise the result.
    fn serialize_body_with(family: ModelFamily, effort: ReasoningEffort) -> serde_json::Value {
        let provider = AnthropicProvider::new("k".into(), "claude-x".into()).with_family(family);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: effort,
        };
        serde_json::to_value(provider.build_request_body(&req)).expect("serializes")
    }

    #[test]
    fn anthropic_default_omits_thinking_field() {
        let body = serialize_body_with(ModelFamily::ClaudeSonnet46, ReasoningEffort::Default);
        assert!(
            body.get("thinking").is_none(),
            "Default must not emit `thinking`: {body}"
        );
    }

    #[test]
    fn anthropic_off_omits_thinking_field() {
        let body = serialize_body_with(ModelFamily::ClaudeSonnet46, ReasoningEffort::Off);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn anthropic_sonnet_low_emits_enabled_with_budget() {
        let body = serialize_body_with(ModelFamily::ClaudeSonnet46, ReasoningEffort::Low);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn anthropic_sonnet_max_clamps_at_64k() {
        let body = serialize_body_with(ModelFamily::ClaudeSonnet46, ReasoningEffort::Max);
        assert_eq!(body["thinking"]["budget_tokens"], 65536);
    }

    #[test]
    fn anthropic_haiku_uses_enabled_shape() {
        let body = serialize_body_with(ModelFamily::ClaudeHaiku45, ReasoningEffort::Medium);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
    }

    #[test]
    fn anthropic_opus_uses_adaptive_for_any_active_level() {
        // Opus 4.7 rejects `type: "enabled"`, so we send adaptive for
        // every Minimal..Max level — the model runs its own policy.
        for eff in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let body = serialize_body_with(ModelFamily::ClaudeOpus47, eff);
            assert_eq!(body["thinking"]["type"], "adaptive", "for {eff:?}");
            assert!(
                body["thinking"].get("budget_tokens").is_none(),
                "adaptive must not carry budget_tokens for {eff:?}: {body}"
            );
        }
    }

    #[test]
    fn anthropic_opus_default_and_off_still_omit() {
        for eff in [ReasoningEffort::Default, ReasoningEffort::Off] {
            let body = serialize_body_with(ModelFamily::ClaudeOpus47, eff);
            assert!(body.get("thinking").is_none(), "for {eff:?}: {body}");
        }
    }
}
