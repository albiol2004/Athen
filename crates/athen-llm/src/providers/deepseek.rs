//! DeepSeek provider adapter.
//!
//! DeepSeek exposes an OpenAI-compatible chat completions API, so this
//! provider builds standard OpenAI-format requests and parses the
//! corresponding responses.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-chat";

/// DeepSeek LLM provider.
pub struct DeepSeekProvider {
    api_key: String,
    default_model: String,
    client: Client,
    base_url: String,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider with the default model (`deepseek-chat`).
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            default_model: DEFAULT_MODEL.to_string(),
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Override the base URL (useful for testing or proxies).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    /// Override the default model.
    pub fn with_model(mut self, model: String) -> Self {
        self.default_model = model;
        self
    }

    /// Override the HTTP client.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Build the OpenAI-compatible request body from our generic request.
    fn build_request_body(&self, request: &LlmRequest) -> OpenAiRequestOut {
        let mut messages: Vec<OpenAiMessageOut> = Vec::new();

        // Prepend system prompt as a system message if provided.
        if let Some(ref system) = request.system_prompt {
            messages.push(OpenAiMessageOut {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(system.clone())),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        for m in &request.messages {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };

            match (&m.role, &m.content) {
                // Assistant messages with embedded tool_calls metadata.
                (Role::Assistant, MessageContent::Structured(v))
                    if v.get("tool_calls").is_some() =>
                {
                    let text = v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    let content = if text.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::String(text.to_string()))
                    };

                    // Convert our ToolCall structs into OpenAI wire format.
                    let tool_calls: Option<Vec<OpenAiToolCallOut>> = v
                        .get("tool_calls")
                        .and_then(|tc| serde_json::from_value::<Vec<ToolCallWire>>(tc.clone()).ok())
                        .map(|calls| {
                            calls
                                .into_iter()
                                .map(|tc| OpenAiToolCallOut {
                                    id: tc.id,
                                    call_type: "function".to_string(),
                                    function: OpenAiToolCallFunctionOut {
                                        name: tc.name,
                                        arguments: if tc.arguments.is_string() {
                                            tc.arguments
                                                .as_str()
                                                .unwrap_or_default()
                                                .to_string()
                                        } else {
                                            serde_json::to_string(&tc.arguments)
                                                .unwrap_or_default()
                                        },
                                    },
                                })
                                .collect()
                        });

                    messages.push(OpenAiMessageOut {
                        role: role.to_string(),
                        content,
                        tool_call_id: None,
                        tool_calls,
                    });
                }
                // Tool result messages with tool_call_id.
                (Role::Tool, MessageContent::Structured(v))
                    if v.get("tool_call_id").is_some() =>
                {
                    let tool_call_id = v
                        .get("tool_call_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let content_str = v
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("{}");

                    messages.push(OpenAiMessageOut {
                        role: role.to_string(),
                        content: Some(serde_json::Value::String(content_str.to_string())),
                        tool_call_id: Some(tool_call_id),
                        tool_calls: None,
                    });
                }
                // All other messages: plain text or structured.
                (_, content) => {
                    let content_value = match content {
                        MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                        MessageContent::Structured(v) => v.clone(),
                    };

                    messages.push(OpenAiMessageOut {
                        role: role.to_string(),
                        content: Some(content_value),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
            }
        }

        // Map tools to OpenAI function-calling format if present.
        let tools = request.tools.as_ref().map(|tool_defs| {
            tool_defs
                .iter()
                .map(|td| OpenAiTool {
                    tool_type: "function".to_string(),
                    function: OpenAiFunction {
                        name: td.name.clone(),
                        description: td.description.clone(),
                        parameters: td.parameters.clone(),
                    },
                })
                .collect()
        });

        OpenAiRequestOut {
            model: self.default_model.clone(),
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature.unwrap_or(0.7),
            tools,
            stream: false,
        }
    }

    /// Map HTTP error responses to `AthenError`.
    fn map_error(&self, status: reqwest::StatusCode, body: &str) -> AthenError {
        let message = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            format!("rate_limit: {}", body)
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            format!("auth error: {}", body)
        } else {
            format!("HTTP {}: {}", status, body)
        };

        AthenError::LlmProvider {
            provider: "deepseek".into(),
            message,
        }
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    fn provider_id(&self) -> &str {
        "deepseek"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request_body(request);
        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!(model = %body.model, "sending DeepSeek completion request");

        let http_response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenError::LlmProvider {
                provider: "deepseek".into(),
                message: format!("request failed: {}", e),
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let api_response: OpenAiResponse =
            http_response.json().await.map_err(|e| AthenError::LlmProvider {
                provider: "deepseek".into(),
                message: format!("failed to parse response: {}", e),
            })?;

        let choice = api_response.choices.first().ok_or_else(|| AthenError::LlmProvider {
            provider: "deepseek".into(),
            message: "response contained no choices".into(),
        })?;

        let content = choice.message.content.clone().unwrap_or_default();
        let reasoning_content = choice.message.reasoning_content.clone();

        // Extract tool calls if present.
        let tool_calls: Vec<ToolCall> = choice
            .message
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::String(tc.function.arguments.clone())),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let finish_reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolUse,
            Some("length") => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };

        let usage = if let Some(ref u) = api_response.usage {
            TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                estimated_cost_usd: Some(estimate_deepseek_cost(
                    &api_response.model,
                    u.prompt_tokens,
                    u.completion_tokens,
                )),
            }
        } else {
            TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                estimated_cost_usd: None,
            }
        };

        Ok(LlmResponse {
            content,
            reasoning_content,
            model_used: api_response.model,
            provider: "deepseek".into(),
            usage,
            tool_calls,
            finish_reason,
        })
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let mut body = self.build_request_body(request);
        body.stream = true;
        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!(model = %body.model, "sending DeepSeek streaming request");

        let http_response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenError::LlmProvider {
                provider: "deepseek".into(),
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
                    provider: "deepseek".into(),
                    message: format!("stream error: {}", e),
                })],
            })
            .flat_map(futures::stream::iter);

        Ok(Box::pin(chunk_stream))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Parse SSE text into `LlmChunk` results (OpenAI streaming format).
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
                    let delta_obj = event
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"));

                    // Check for reasoning_content (DeepSeek R1 thinking output).
                    if let Some(reasoning) = delta_obj
                        .and_then(|d| d.get("reasoning_content"))
                        .and_then(|c| c.as_str())
                    {
                        if !reasoning.is_empty() {
                            chunks.push(Ok(LlmChunk {
                                delta: reasoning.to_string(),
                                is_final: false,
                                is_thinking: true,
                                tool_calls: vec![],
                            }));
                        }
                    }

                    // OpenAI streaming format: choices[0].delta.content
                    if let Some(delta_content) = delta_obj
                        .and_then(|d| d.get("content"))
                        .and_then(|c| c.as_str())
                    {
                        chunks.push(Ok(LlmChunk {
                            delta: delta_content.to_string(),
                            is_final: false,
                            is_thinking: false,
                            tool_calls: vec![],
                        }));
                    }

                    // Check for tool_calls in the delta.
                    if let Some(tool_calls_arr) = delta_obj
                        .and_then(|d| d.get("tool_calls"))
                        .and_then(|tc| tc.as_array())
                    {
                        let mut extracted_calls = Vec::new();
                        for tc_val in tool_calls_arr {
                            if let (Some(id), Some(name), Some(args_str)) = (
                                tc_val.get("id").and_then(|v| v.as_str()),
                                tc_val.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()),
                                tc_val.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()),
                            ) {
                                let arguments = serde_json::from_str(args_str)
                                    .unwrap_or(serde_json::Value::String(args_str.to_string()));
                                extracted_calls.push(ToolCall {
                                    id: id.to_string(),
                                    name: name.to_string(),
                                    arguments,
                                });
                            }
                        }
                        if !extracted_calls.is_empty() {
                            chunks.push(Ok(LlmChunk {
                                delta: String::new(),
                                is_final: false,
                                is_thinking: false,
                                tool_calls: extracted_calls,
                            }));
                        }
                    }

                    // Check for finish_reason to detect the final chunk.
                    if let Some(finish) = event
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|f| f.as_str())
                    {
                        if finish == "stop" || finish == "length" || finish == "tool_calls" {
                            chunks.push(Ok(LlmChunk {
                                delta: String::new(),
                                is_final: true,
                                is_thinking: false,
                                tool_calls: vec![],
                            }));
                        }
                    }
                }
                Err(_) => {
                    warn!(data = data, "failed to parse DeepSeek SSE event data");
                }
            }
        }
    }

    chunks
}

/// Cost estimation for DeepSeek models.
///
/// DeepSeek pricing is very competitive (as of 2025):
/// - deepseek-chat: ~$0.14/M input, ~$0.28/M output
/// - deepseek-reasoner: ~$0.55/M input, ~$2.19/M output
fn estimate_deepseek_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("reasoner") {
        (0.55, 2.19)
    } else {
        // deepseek-chat and other models
        (0.14, 0.28)
    };

    let input_cost = (input_tokens as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_per_m;
    input_cost + output_cost
}

// ---------------------------------------------------------------------------
// OpenAI-compatible API types (used by DeepSeek)
// ---------------------------------------------------------------------------

/// Helper for deserializing tool calls from our structured ChatMessage content.
#[derive(Debug, Deserialize)]
struct ToolCallWire {
    id: String,
    name: String,
    arguments: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Outbound (serialized) API types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OpenAiRequestOut {
    model: String,
    messages: Vec<OpenAiMessageOut>,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct OpenAiMessageOut {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCallOut>>,
}

#[derive(Debug, Serialize)]
struct OpenAiToolCallOut {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAiToolCallFunctionOut,
}

#[derive(Debug, Serialize)]
struct OpenAiToolCallFunctionOut {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAiFunction,
}

#[derive(Debug, Serialize)]
struct OpenAiFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    model: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCall {
    id: String,
    function: OpenAiToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}
