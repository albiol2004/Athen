//! Anthropic (Claude) provider adapter.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

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
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(api_key: String, default_model: String) -> Self {
        Self {
            api_key,
            default_model,
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            supports_vision: false,
            supports_documents: false,
        }
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
                let content = match &m.content {
                    MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                    MessageContent::Structured(v) => v.clone(),
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
            .map_err(|e| AthenError::LlmProvider {
                provider: "anthropic".into(),
                message: format!("request failed: {}", e),
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
        };

        Ok(LlmResponse {
            content,
            reasoning_content: None,
            model_used: api_response.model,
            provider: "anthropic".into(),
            usage,
            tool_calls,
            finish_reason,
        })
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
            .map_err(|e| AthenError::LlmProvider {
                provider: "anthropic".into(),
                message: format!("streaming request failed: {}", e),
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let byte_stream = http_response.bytes_stream();

        // Parse SSE events from the byte stream.
        let chunk_stream = byte_stream
            .map(|result| match result {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    parse_sse_chunks(&text)
                }
                Err(e) => vec![Err(AthenError::LlmProvider {
                    provider: "anthropic".into(),
                    message: format!("stream error: {}", e),
                })],
            })
            .flat_map(futures::stream::iter);

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
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        let blocks = body.messages[0].content.as_array().expect("array content");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
    }
}
