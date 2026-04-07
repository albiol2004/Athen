//! Generic OpenAI-compatible provider adapter.
//!
//! Works with any server that exposes the OpenAI chat completions API:
//! OpenAI itself, Ollama, llama.cpp, LM Studio, vLLM, text-generation-webui,
//! and any other compatible endpoint.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "gpt-4o";

/// A fully generic OpenAI-compatible LLM provider.
///
/// This works with any server that implements the `/v1/chat/completions`
/// endpoint using the OpenAI wire format. The API key is optional — local
/// servers (Ollama, llama.cpp, LM Studio) typically don't require one.
///
/// # Examples
///
/// ```rust,no_run
/// use athen_llm::providers::openai::OpenAiCompatibleProvider;
///
/// // OpenAI proper
/// let openai = OpenAiCompatibleProvider::new("https://api.openai.com".into())
///     .with_api_key("sk-...".into())
///     .with_model("gpt-4o".into());
///
/// // Local llama.cpp server (no auth)
/// let local = OpenAiCompatibleProvider::new("http://localhost:8080".into())
///     .with_model("my-model".into())
///     .with_provider_id("llamacpp".into());
/// ```
pub struct OpenAiCompatibleProvider {
    api_key: Option<String>,
    default_model: String,
    client: Client,
    base_url: String,
    provider_id: String,
    cost_estimator: Box<dyn CostEstimator>,
}

impl OpenAiCompatibleProvider {
    /// Create a new provider pointing at the given base URL.
    ///
    /// No API key is set by default — add one with [`with_api_key`] if the
    /// server requires authentication.
    pub fn new(base_url: String) -> Self {
        Self {
            api_key: None,
            default_model: DEFAULT_MODEL.to_string(),
            client: Client::new(),
            base_url,
            provider_id: "openai".to_string(),
            cost_estimator: Box::new(OpenAiCostEstimator),
        }
    }

    /// Convenience constructor for OpenAI proper (with API key).
    pub fn openai(api_key: String) -> Self {
        Self::new(DEFAULT_BASE_URL.to_string())
            .with_api_key(api_key)
            .with_model(DEFAULT_MODEL.to_string())
    }

    /// Set the API key. The `Authorization: Bearer <key>` header is only
    /// sent when an API key is present.
    pub fn with_api_key(mut self, api_key: String) -> Self {
        self.api_key = Some(api_key);
        self
    }

    /// Override the default model name.
    pub fn with_model(mut self, model: String) -> Self {
        self.default_model = model;
        self
    }

    /// Override the provider identifier returned by [`LlmProvider::provider_id`].
    pub fn with_provider_id(mut self, id: String) -> Self {
        self.provider_id = id;
        self
    }

    /// Override the HTTP client (useful for testing or custom TLS configs).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Override the cost estimator (e.g. zero-cost for local providers).
    pub fn with_cost_estimator(mut self, estimator: Box<dyn CostEstimator>) -> Self {
        self.cost_estimator = estimator;
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
                        .and_then(|tc| {
                            serde_json::from_value::<Vec<ToolCallWire>>(tc.clone()).ok()
                        })
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
            extra: None,
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
            provider: self.provider_id.clone(),
            message,
        }
    }

    /// Build the HTTP request, conditionally adding the auth header.
    fn build_http_request(&self, url: &str, body: &OpenAiRequestOut) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(body);

        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        req
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request_body(request);
        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!(
            provider = %self.provider_id,
            model = %body.model,
            "sending completion request"
        );

        let http_response = self
            .build_http_request(&url, &body)
            .send()
            .await
            .map_err(|e| AthenError::LlmProvider {
                provider: self.provider_id.clone(),
                message: format!("request failed: {}", e),
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let api_response: OpenAiResponse =
            http_response
                .json()
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: self.provider_id.clone(),
                    message: format!("failed to parse response: {}", e),
                })?;

        let choice = api_response
            .choices
            .first()
            .ok_or_else(|| AthenError::LlmProvider {
                provider: self.provider_id.clone(),
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
                            .unwrap_or(serde_json::Value::String(
                                tc.function.arguments.clone(),
                            )),
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
                estimated_cost_usd: Some(self.cost_estimator.estimate(
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
            provider: self.provider_id.clone(),
            usage,
            tool_calls,
            finish_reason,
        })
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let mut body = self.build_request_body(request);
        body.stream = true;
        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!(
            provider = %self.provider_id,
            model = %body.model,
            "sending streaming request"
        );

        let http_response = self
            .build_http_request(&url, &body)
            .send()
            .await
            .map_err(|e| AthenError::LlmProvider {
                provider: self.provider_id.clone(),
                message: format!("streaming request failed: {}", e),
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let byte_stream = http_response.bytes_stream();
        let provider_id = self.provider_id.clone();

        // Parse SSE events from the byte stream.
        let chunk_stream = byte_stream
            .map(move |result| match result {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    parse_sse_chunks(&text, &provider_id)
                }
                Err(e) => vec![Err(AthenError::LlmProvider {
                    provider: provider_id.clone(),
                    message: format!("stream error: {}", e),
                })],
            })
            .flat_map(futures::stream::iter);

        Ok(Box::pin(chunk_stream))
    }

    async fn is_available(&self) -> bool {
        // Cloud providers with an API key are assumed available.
        // Override this for local providers that need a health check.
        self.api_key.is_some()
    }
}

/// Parse SSE text into `LlmChunk` results (OpenAI streaming format).
///
/// This is public so wrapper providers can reuse it if needed.
pub fn parse_sse_chunks(text: &str, provider_id: &str) -> Vec<Result<LlmChunk>> {
    let mut chunks = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            debug!(provider = provider_id, raw_sse = data, "SSE chunk received");
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

                    // Check for reasoning_content (thinking models like Qwen 3.5, DeepSeek R1).
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
                    warn!(
                        provider = provider_id,
                        data = data,
                        "failed to parse SSE event data"
                    );
                }
            }
        }
    }

    chunks
}

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

/// Trait for pluggable cost estimation per provider.
///
/// Implement this to provide accurate pricing for different providers.
/// Local providers should return 0.0 for all models.
pub trait CostEstimator: Send + Sync {
    /// Estimate the USD cost for a completion given the model name and token counts.
    fn estimate(&self, model: &str, input_tokens: u32, output_tokens: u32) -> f64;
}

/// OpenAI pricing (as of early 2025).
pub struct OpenAiCostEstimator;

impl CostEstimator for OpenAiCostEstimator {
    fn estimate(&self, model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        let (input_per_m, output_per_m) = if model.contains("gpt-4o-mini") {
            // gpt-4o-mini
            (0.15, 0.60)
        } else if model.contains("gpt-4o") {
            // gpt-4o
            (2.50, 10.00)
        } else if model.contains("gpt-4-turbo") {
            (10.00, 30.00)
        } else if model.contains("o3-mini") {
            (1.10, 4.40)
        } else if model.contains("o3") {
            (10.00, 40.00)
        } else if model.contains("o1-mini") {
            (1.10, 4.40)
        } else if model.contains("o1") {
            (15.00, 60.00)
        } else if model.contains("gpt-3.5") {
            (0.50, 1.50)
        } else {
            // Unknown model — use gpt-4o pricing as a safe default.
            (2.50, 10.00)
        };

        let input_cost = (input_tokens as f64 / 1_000_000.0) * input_per_m;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * output_per_m;
        input_cost + output_cost
    }
}

/// Zero-cost estimator for local providers (Ollama, llama.cpp, etc.).
pub struct ZeroCostEstimator;

impl CostEstimator for ZeroCostEstimator {
    fn estimate(&self, _model: &str, _input_tokens: u32, _output_tokens: u32) -> f64 {
        0.0
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible API wire types
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
    /// Extra fields merged into the top-level request body.
    /// Used for provider-specific parameters like Qwen's `enable_thinking`.
    #[serde(flatten)]
    extra: Option<serde_json::Value>,
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

// ---------------------------------------------------------------------------
// Inbound (deserialized) API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiResponse {
    pub model: String,
    pub choices: Vec<OpenAiChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiChoice {
    pub message: OpenAiResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiResponseMessage {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<OpenAiToolCallIn>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiToolCallIn {
    pub id: String,
    pub function: OpenAiToolCallFunctionIn,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiToolCallFunctionIn {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::tool::ToolDefinition;

    fn make_provider() -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new("https://api.openai.com".into())
            .with_api_key("sk-test-key".into())
            .with_model("gpt-4o".into())
    }

    fn make_provider_no_auth() -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new("http://localhost:8080".into())
            .with_model("my-local-model".into())
            .with_provider_id("llamacpp".into())
            .with_cost_estimator(Box::new(ZeroCostEstimator))
    }

    fn simple_request() -> LlmRequest {
        LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("Hello".into()),
            }],
            max_tokens: Some(1024),
            temperature: Some(0.5),
            tools: None,
            system_prompt: None,
        }
    }

    #[test]
    fn test_build_request_basic() {
        let provider = make_provider();
        let request = simple_request();
        let body = provider.build_request_body(&request);

        assert_eq!(body.model, "gpt-4o");
        assert_eq!(body.max_tokens, 1024);
        assert_eq!(body.temperature, 0.5);
        assert!(!body.stream);
        assert!(body.tools.is_none());
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn test_build_request_with_system_prompt() {
        let provider = make_provider();
        let mut request = simple_request();
        request.system_prompt = Some("You are helpful.".into());
        let body = provider.build_request_body(&request);

        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(
            body.messages[0].content,
            Some(serde_json::Value::String("You are helpful.".into()))
        );
    }

    #[test]
    fn test_build_request_with_tools() {
        let provider = make_provider();
        let mut request = simple_request();
        request.tools = Some(vec![ToolDefinition {
            name: "calculator".into(),
            description: "Does math".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {"type": "string"}
                }
            }),
            backend: athen_core::tool::ToolBackend::Shell {
                command: "echo".into(),
                native: false,
            },
            base_risk: athen_core::risk::BaseImpact::Read,
        }]);
        let body = provider.build_request_body(&request);

        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "calculator");
        assert_eq!(tools[0].tool_type, "function");
    }

    #[test]
    fn test_build_request_defaults() {
        let provider = make_provider();
        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("Hi".into()),
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
        };
        let body = provider.build_request_body(&request);

        assert_eq!(body.max_tokens, 4096);
        assert_eq!(body.temperature, 0.7);
    }

    #[test]
    fn test_build_request_tool_result_message() {
        let provider = make_provider();
        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::Tool,
                content: MessageContent::Structured(serde_json::json!({
                    "tool_call_id": "call_123",
                    "content": "{\"result\": 42}"
                })),
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
        };
        let body = provider.build_request_body(&request);

        assert_eq!(body.messages[0].role, "tool");
        assert_eq!(
            body.messages[0].tool_call_id,
            Some("call_123".to_string())
        );
        assert_eq!(
            body.messages[0].content,
            Some(serde_json::Value::String("{\"result\": 42}".into()))
        );
    }

    #[test]
    fn test_build_request_assistant_tool_calls() {
        let provider = make_provider();
        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::Assistant,
                content: MessageContent::Structured(serde_json::json!({
                    "text": "",
                    "tool_calls": [
                        {"id": "call_1", "name": "calculator", "arguments": {"x": 1}}
                    ]
                })),
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
        };
        let body = provider.build_request_body(&request);

        assert_eq!(body.messages[0].role, "assistant");
        assert!(body.messages[0].content.is_none()); // empty text -> None
        let tc = body.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].function.name, "calculator");
    }

    #[test]
    fn test_provider_id_configurable() {
        let p1 = make_provider();
        assert_eq!(p1.provider_id(), "openai");

        let p2 = make_provider_no_auth();
        assert_eq!(p2.provider_id(), "llamacpp");
    }

    #[test]
    fn test_parse_sse_chunks_content() {
        let sse = r#"data: {"choices":[{"delta":{"content":"Hello"},"index":0}]}

data: {"choices":[{"delta":{"content":" world"},"index":0}]}

data: [DONE]
"#;
        let chunks = parse_sse_chunks(sse, "test");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].as_ref().unwrap().delta, "Hello");
        assert!(!chunks[0].as_ref().unwrap().is_final);
        assert_eq!(chunks[1].as_ref().unwrap().delta, " world");
        assert!(chunks[2].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_parse_sse_chunks_finish_reason() {
        let sse =
            r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#;
        let chunks = parse_sse_chunks(sse, "test");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_parse_sse_ignores_non_data_lines() {
        let sse = ": this is a comment\nsome random line\ndata: [DONE]\n";
        let chunks = parse_sse_chunks(sse, "test");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_openai_cost_estimator() {
        let est = OpenAiCostEstimator;

        // gpt-4o: $2.50/M input, $10.00/M output
        let cost = est.estimate("gpt-4o", 1_000_000, 1_000_000);
        assert!((cost - 12.50).abs() < 0.001);

        // gpt-4o-mini: $0.15/M input, $0.60/M output
        let cost = est.estimate("gpt-4o-mini", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_zero_cost_estimator() {
        let est = ZeroCostEstimator;
        assert_eq!(est.estimate("any-model", 1_000_000, 1_000_000), 0.0);
    }

    #[tokio::test]
    async fn test_is_available_with_key() {
        let provider = make_provider();
        assert!(provider.is_available().await);
    }

    #[tokio::test]
    async fn test_is_available_without_key() {
        let provider =
            OpenAiCompatibleProvider::new("http://localhost:8080".into());
        assert!(!provider.is_available().await);
    }

    #[test]
    fn test_map_error_rate_limit() {
        let provider = make_provider();
        let err = provider.map_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down");
        match err {
            AthenError::LlmProvider { provider, message } => {
                assert_eq!(provider, "openai");
                assert!(message.contains("rate_limit"));
            }
            _ => panic!("unexpected error variant"),
        }
    }

    #[test]
    fn test_map_error_auth() {
        let provider = make_provider();
        let err = provider.map_error(reqwest::StatusCode::UNAUTHORIZED, "bad key");
        match err {
            AthenError::LlmProvider { provider, message } => {
                assert_eq!(provider, "openai");
                assert!(message.contains("auth error"));
            }
            _ => panic!("unexpected error variant"),
        }
    }

    #[test]
    fn test_openai_convenience_constructor() {
        let provider = OpenAiCompatibleProvider::openai("sk-test".into());
        assert_eq!(provider.provider_id(), "openai");
        assert_eq!(provider.default_model, "gpt-4o");
        assert_eq!(provider.base_url, "https://api.openai.com");
        assert_eq!(provider.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn test_response_parsing() {
        let json = r#"{
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let resp: OpenAiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.model, "gpt-4o");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, 10);
    }

    #[test]
    fn test_response_parsing_with_tool_calls() {
        let json = r#"{
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "shell_execute",
                            "arguments": "{\"command\":\"echo hi\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 10,
                "total_tokens": 30
            }
        }"#;

        let resp: OpenAiResponse = serde_json::from_str(json).unwrap();
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_abc");
        assert_eq!(tc[0].function.name, "shell_execute");
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }
}
