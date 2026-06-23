//! DeepSeek provider adapter.
//!
//! DeepSeek exposes an OpenAI-compatible chat completions API, so this
//! provider builds standard OpenAI-format requests and parses the
//! corresponding responses.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

use crate::providers::openai::{
    parse_sse_chunks, parse_tool_arguments, take_complete_lines, ToolCallAccumulator,
};
use crate::quirks::{self, seed, ModelQuirks};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-chat";

/// DeepSeek LLM provider.
pub struct DeepSeekProvider {
    api_key: String,
    default_model: String,
    client: Client,
    base_url: String,
    /// Resolved at construction from the user-selected `ModelFamily`.
    /// Defaults to the `DeepSeekV4Chat` quirks (control-char repair, no
    /// reasoning); switch to `DeepSeekR1` to enable reasoning_content
    /// promotion + echo-on-tool-turn.
    quirks: ModelQuirks,
    /// Family kept alongside `quirks` so request-shaping (specifically the
    /// V4 Flash `"thinking": {"type": "disabled"}` knob) can branch on it.
    family: ModelFamily,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider with the default model (`deepseek-chat`).
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            default_model: DEFAULT_MODEL.to_string(),
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest Client should build with timeout"),
            base_url: DEFAULT_BASE_URL.to_string(),
            // DeepSeek-V4 chat is the typical default for this provider.
            // Callers can override via `with_family(ModelFamily::DeepSeekR1)`
            // if they're pointed at the reasoner endpoint.
            quirks: seed::quirks_for_family(ModelFamily::DeepSeekV4Chat),
            family: ModelFamily::DeepSeekV4Chat,
        }
    }

    /// Set the model family. The default constructor seeds
    /// `DeepSeekV4Chat`; switch to `DeepSeekR1` when pointing at the
    /// reasoner endpoint so reasoning_content is promoted and echoed.
    pub fn with_family(mut self, family: ModelFamily) -> Self {
        self.quirks = seed::quirks_for_family(family);
        self.family = family;
        self
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
                reasoning_content: None,
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
                // Assistant messages stored as a Structured envelope —
                // carries tool_calls and/or reasoning_content alongside the
                // text. DeepSeek thinking-mode (V4 with reasoning_effort
                // set, or DeepSeek-R1) returns HTTP 400 on the next call if
                // the prior turn's reasoning_content isn't echoed back, so
                // we round-trip it through this envelope.
                (Role::Assistant, MessageContent::Structured(v))
                    if v.get("tool_calls").is_some()
                        || v.get("reasoning_content").is_some()
                        || v.get("text").is_some() =>
                {
                    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                    let content = if text.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::String(text.to_string()))
                    };

                    let reasoning_content = v
                        .get("reasoning_content")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());

                    // Convert our ToolCall structs into OpenAI wire format.
                    // Empty / missing arrays serialize to `None` so the field
                    // is omitted entirely.
                    let tool_calls: Option<Vec<OpenAiToolCallOut>> = v
                        .get("tool_calls")
                        .and_then(|tc| serde_json::from_value::<Vec<ToolCallWire>>(tc.clone()).ok())
                        .filter(|calls| !calls.is_empty())
                        .map(|calls| {
                            calls
                                .into_iter()
                                .map(|tc| OpenAiToolCallOut {
                                    id: tc.id,
                                    call_type: "function".to_string(),
                                    function: OpenAiToolCallFunctionOut {
                                        name: tc.name,
                                        arguments: if tc.arguments.is_string() {
                                            tc.arguments.as_str().unwrap_or_default().to_string()
                                        } else {
                                            serde_json::to_string(&tc.arguments).unwrap_or_default()
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
                        reasoning_content,
                    });
                }
                // Tool result messages with tool_call_id.
                (Role::Tool, MessageContent::Structured(v)) if v.get("tool_call_id").is_some() => {
                    let tool_call_id = v
                        .get("tool_call_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let content_str = v.get("content").and_then(|c| c.as_str()).unwrap_or("{}");

                    messages.push(OpenAiMessageOut {
                        role: role.to_string(),
                        content: Some(serde_json::Value::String(content_str.to_string())),
                        tool_call_id: Some(tool_call_id),
                        tool_calls: None,
                        reasoning_content: None,
                    });
                }
                // All other messages: plain text or structured. Multimodal
                // is rejected upstream by `reject_multimodal` before we ever
                // get here; we degrade to the text-only fallback if it
                // somehow slips through, never silently dropping images
                // (the upstream rejection is the contract).
                (_, content) => {
                    let content_value = match content {
                        MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                        MessageContent::Structured(v) => v.clone(),
                        MessageContent::Multimodal { text, .. } => {
                            serde_json::Value::String(text.clone())
                        }
                    };

                    messages.push(OpenAiMessageOut {
                        role: role.to_string(),
                        content: Some(content_value),
                        tool_call_id: None,
                        tool_calls: None,
                        reasoning_content: None,
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

        let reasoning_effort = map_deepseek_reasoning_effort(request.reasoning_effort);
        // DeepSeek V4 Flash defaults to thinking-on, regardless of whether
        // `reasoning_effort` is sent. The only way to get a pure non-thinking
        // response shape is to opt out via `"thinking": {"type": "disabled"}`.
        // Emit it when the family is `DeepSeekV4Chat` (the "chat / non-thinking"
        // semantic) AND no explicit reasoning effort was requested. Callers who
        // want reasoning on V4 should pass any non-default effort, or pick
        // `DeepSeekV4Pro` / `DeepSeekR1` which keep thinking on.
        let thinking = match (self.family, request.reasoning_effort) {
            (ModelFamily::DeepSeekV4Chat, ReasoningEffort::Default | ReasoningEffort::Off) => {
                Some(serde_json::json!({ "type": "disabled" }))
            }
            _ => None,
        };

        OpenAiRequestOut {
            model: self.default_model.clone(),
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature.unwrap_or(0.7),
            tools,
            stream: false,
            stream_options: None,
            reasoning_effort,
            thinking,
        }
    }

    /// Map HTTP error responses to `AthenError`.
    fn map_error(
        &self,
        status: reqwest::StatusCode,
        body: &str,
        retry_after_secs: Option<u64>,
    ) -> AthenError {
        if crate::providers::is_transient_status(status) {
            let message = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                format!("rate_limit: {}", body)
            } else {
                format!("server overloaded ({}): {}", status, body)
            };
            return AthenError::LlmTransient {
                provider: "deepseek".into(),
                message,
                retry_after_secs,
            };
        }

        let message = if status == reqwest::StatusCode::UNAUTHORIZED {
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
        crate::providers::reject_multimodal("deepseek", request)?;
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
            .map_err(|e| crate::providers::map_send_error("deepseek", "request failed", e))?;

        let status = http_response.status();
        if !status.is_success() {
            let retry_after = crate::providers::parse_retry_after(http_response.headers());
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body, retry_after));
        }

        let api_response: OpenAiResponse =
            http_response
                .json()
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: "deepseek".into(),
                    message: format!("failed to parse response: {}", e),
                })?;

        let choice = api_response
            .choices
            .first()
            .ok_or_else(|| AthenError::LlmProvider {
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
                        arguments: parse_tool_arguments(&tc.function.arguments),
                        thought_signature: None,
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
            let cache_hit = u.prompt_cache_hit_tokens.unwrap_or(0);
            TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                estimated_cost_usd: Some(estimate_deepseek_cost(
                    &api_response.model,
                    u.prompt_tokens,
                    u.completion_tokens,
                    cache_hit,
                )),
                cached_tokens: u.prompt_cache_hit_tokens,
                cache_creation_tokens: None,
            }
        } else {
            TokenUsage::default()
        };

        let mut response = LlmResponse {
            content,
            reasoning_content,
            model_used: api_response.model,
            provider: "deepseek".into(),
            usage,
            tool_calls,
            finish_reason,
        };
        quirks::apply_to_response(&self.quirks, &mut response);
        Ok(response)
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        crate::providers::reject_multimodal("deepseek", request)?;
        let mut body = self.build_request_body(request);
        body.stream = true;
        body.stream_options = Some(StreamOptions {
            include_usage: true,
        });
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
            .map_err(|e| {
                crate::providers::map_send_error("deepseek", "streaming request failed", e)
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let retry_after = crate::providers::parse_retry_after(http_response.headers());
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body, retry_after));
        }

        let byte_stream = http_response.bytes_stream();
        let quirks_snapshot = self.quirks;

        // DeepSeek is OpenAI-compatible — reuse the stateful SSE parser so
        // fragmented tool-call deltas are correctly assembled across chunks,
        // and buffer raw bytes across HTTP chunks so SSE lines and multi-byte
        // UTF-8 codepoints split at chunk boundaries reassemble correctly.
        // The `content_buffer` + `saw_structured` fields drive the
        // end-of-stream inline tool-call extraction (a no-op for the
        // baseline `Structured` strategy DeepSeek's chat-class models use).
        let chunk_stream = futures::stream::unfold(
            DeepSeekStreamState {
                byte_stream,
                acc: ToolCallAccumulator::default(),
                pending: Vec::<u8>::new(),
                done: false,
                content_buffer: String::new(),
                saw_structured: false,
                quirks: quirks_snapshot,
            },
            |mut s| async move {
                if s.done {
                    return None;
                }
                let parsed = match s.byte_stream.next().await {
                    Some(Ok(bytes)) => {
                        s.pending.extend_from_slice(&bytes);
                        let complete = take_complete_lines(&mut s.pending);
                        if complete.is_empty() {
                            Vec::new()
                        } else {
                            let text = String::from_utf8_lossy(&complete);
                            let chunks = parse_sse_chunks(&text, "deepseek", &mut s.acc);
                            observe_deepseek_chunks(&chunks, &mut s);
                            chunks
                        }
                    }
                    Some(Err(e)) => vec![Err(AthenError::LlmProvider {
                        provider: "deepseek".into(),
                        message: format!("stream error: {}", e),
                    })],
                    None => {
                        s.done = true;
                        let mut out = Vec::new();
                        if !s.pending.is_empty() {
                            let tail = std::mem::take(&mut s.pending);
                            let text = String::from_utf8_lossy(&tail);
                            let chunks = parse_sse_chunks(&text, "deepseek", &mut s.acc);
                            observe_deepseek_chunks(&chunks, &mut s);
                            out.extend(chunks);
                        }
                        if !s.acc.is_empty() {
                            let tool_calls = s.acc.drain();
                            if !tool_calls.is_empty() {
                                s.saw_structured = true;
                                out.push(Ok(LlmChunk {
                                    delta: String::new(),
                                    is_final: true,
                                    is_thinking: false,
                                    tool_calls,
                                    usage: None,
                                }));
                            }
                        }
                        if let Some(tail) = quirks::extract_streaming_tail(
                            &s.quirks,
                            &s.content_buffer,
                            s.saw_structured,
                        ) {
                            out.push(Ok(tail));
                        }
                        out
                    }
                };
                Some((futures::stream::iter(parsed), s))
            },
        )
        .flatten();

        // Fill `estimated_cost_usd` on the usage-bearing chunk, billing the
        // cached prefix at DeepSeek's discounted rate — mirrors the
        // non-streaming `complete()` path. `parse_stream_usage` (shared with
        // OpenAI) already mapped `prompt_cache_hit_tokens` → `cached_tokens`.
        let model = self.default_model.clone();
        let chunk_stream = chunk_stream.map(move |item| match item {
            Ok(mut chunk) => {
                if let Some(usage) = chunk.usage.as_mut() {
                    if usage.estimated_cost_usd.is_none() {
                        let cache_hit = usage.cached_tokens.unwrap_or(0);
                        usage.estimated_cost_usd = Some(estimate_deepseek_cost(
                            &model,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            cache_hit,
                        ));
                    }
                }
                Ok(chunk)
            }
            err => err,
        });

        Ok(Box::pin(chunk_stream))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Streaming state for DeepSeek's `complete_streaming` unfold. Mirrors
/// `OpenAiCompatibleProvider::StreamState` but flattens the byte-stream
/// generic so the unfold closure stays inferable.
struct DeepSeekStreamState<S> {
    byte_stream: S,
    acc: ToolCallAccumulator,
    pending: Vec<u8>,
    done: bool,
    content_buffer: String,
    saw_structured: bool,
    quirks: ModelQuirks,
}

/// Per-batch chunk observer: appends visible deltas to the content buffer
/// (only when extraction will be needed at end-of-stream) and flips the
/// `saw_structured` flag on any chunk that surfaced a structured tool call.
fn observe_deepseek_chunks<S>(
    chunks: &[Result<athen_core::llm::LlmChunk>],
    state: &mut DeepSeekStreamState<S>,
) {
    let needs_buffer = !matches!(
        state.quirks.tool_extraction,
        crate::quirks::ToolExtractionStrategy::Structured
    );
    for c in chunks.iter().flatten() {
        if !c.tool_calls.is_empty() {
            state.saw_structured = true;
        }
        if needs_buffer && !c.delta.is_empty() && !c.is_thinking {
            state.content_buffer.push_str(&c.delta);
        }
    }
}

/// Cost estimation for DeepSeek models.
///
/// DeepSeek pricing is very competitive (as of 2025):
/// - deepseek-chat: ~$0.14/M input miss, ~$0.014/M input hit, ~$0.28/M output
/// - deepseek-reasoner: ~$0.55/M input miss, ~$0.055/M input hit, ~$2.19/M output
///
/// Cache hits bill at ~10% of the miss rate. `cache_hit_tokens` is the
/// portion of `input_tokens` already served from cache; we split the
/// input total into miss + hit and bill each at its own rate. Pass `0`
/// when the response didn't include `prompt_cache_hit_tokens` (the prior
/// behaviour billed everything at the miss rate, overstating cost by up
/// to 10× whenever a hit landed).
fn estimate_deepseek_cost(
    model: &str,
    input_tokens: u32,
    output_tokens: u32,
    cache_hit_tokens: u32,
) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("reasoner") {
        (0.55, 2.19)
    } else {
        // deepseek-chat and other models
        (0.14, 0.28)
    };
    let cache_hit_per_m = input_per_m / 10.0;

    let hit = cache_hit_tokens.min(input_tokens);
    let miss = input_tokens.saturating_sub(hit);

    let input_cost =
        (miss as f64 / 1_000_000.0) * input_per_m + (hit as f64 / 1_000_000.0) * cache_hit_per_m;
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
    /// Opt into the terminal usage-only SSE chunk while streaming (DeepSeek
    /// is OpenAI-compatible). Without it the streamed turn reports no token
    /// counts and the budget never decrements. Omitted for non-streaming.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    /// V4 Flash defaults to thinking-on. Send `{"type": "disabled"}` to
    /// get a clean non-thinking response. Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// DeepSeek V4 chat exposes `reasoning_effort` with only two valid
/// values: `"high"` and `"max"`. Omission disables reasoning entirely.
/// Per the design doc:
/// - Default / Off → omit (DeepSeek defaults to reasoning off)
/// - Minimal / Low / Medium / High → `"high"` (the lowest-on value)
/// - Max → `"max"`
///
/// Models with reasoning baked in (DeepSeek-R1) silently ignore the
/// field on the wire, so this mapping is safe to apply per-request
/// without family branching.
fn map_deepseek_reasoning_effort(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Default | ReasoningEffort::Off => None,
        ReasoningEffort::Minimal
        | ReasoningEffort::Low
        | ReasoningEffort::Medium
        | ReasoningEffort::High => Some("high"),
        ReasoningEffort::Max => Some("max"),
    }
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
    /// Mandatory echo on the prior assistant turn when DeepSeek
    /// thinking-mode is active — sending without it returns HTTP 400
    /// ("The reasoning_content in the thinking mode must be passed back
    /// to the API."). Omitted from the wire when `None` so non-thinking
    /// turns stay byte-identical to the legacy shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
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
    /// DeepSeek-specific: portion of `prompt_tokens` served from the
    /// on-disk KV cache. Caching is fully automatic (64-token block
    /// granularity, exact-prefix match) — we just need to read the
    /// counter back to bill it at the discounted rate and surface the
    /// hit count to the UI. Absent on older responses, hence `default`.
    /// Ref: <https://api-docs.deepseek.com/guides/kv_cache>.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    /// DeepSeek-specific: complement of `prompt_cache_hit_tokens`. We
    /// recompute it locally as `prompt_tokens - cache_hit` if absent,
    /// so this is informational only.
    #[serde(default)]
    #[allow(dead_code)]
    prompt_cache_miss_tokens: Option<u32>,
}

#[cfg(test)]
mod arg_parse_tests {
    use super::*;

    #[test]
    fn well_formed_json_is_parsed_directly() {
        let v = parse_tool_arguments(r#"{"path":"/tmp/x","content":"hi"}"#);
        assert_eq!(v["path"], "/tmp/x");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn raw_newlines_in_string_value_are_repaired() {
        // This is what DeepSeek emits: a literal LF inside the content
        // value, which serde_json::from_str rejects per the JSON spec.
        let raw = "{\"path\":\"/tmp/x.html\",\"content\":\"<html>\nhi\n</html>\"}";
        let v = parse_tool_arguments(raw);
        assert_eq!(v["path"], "/tmp/x.html");
        assert_eq!(v["content"], "<html>\nhi\n</html>");
    }

    #[test]
    fn raw_tabs_and_returns_in_string_value_are_repaired() {
        let raw = "{\"path\":\"/x\",\"content\":\"a\tb\rc\"}";
        let v = parse_tool_arguments(raw);
        assert_eq!(v["content"], "a\tb\rc");
    }

    #[test]
    fn already_escaped_sequences_are_preserved() {
        let raw = r#"{"path":"/x","content":"line1\nline2"}"#;
        let v = parse_tool_arguments(raw);
        assert_eq!(v["content"], "line1\nline2");
    }

    #[test]
    fn unrepairable_input_falls_back_to_string_wrapper() {
        let v = parse_tool_arguments("not even close to json {");
        assert!(v.is_string());
    }

    #[test]
    fn control_chars_outside_strings_are_left_alone() {
        // Newlines between fields are valid JSON whitespace. Make sure
        // we don't munge them.
        let raw = "{\n  \"path\": \"/x\",\n  \"content\": \"hi\"\n}";
        let v = parse_tool_arguments(raw);
        assert_eq!(v["path"], "/x");
    }

    #[tokio::test]
    async fn complete_rejects_multimodal_with_clear_error() {
        let provider = DeepSeekProvider::new("test-key".to_string());
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Multimodal {
                    text: "describe this".to_string(),
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
        let err = provider.complete(&req).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("does not support image"),
            "expected vision rejection error, got: {msg}"
        );
    }

    fn deepseek_body_with(effort: ReasoningEffort) -> serde_json::Value {
        let provider = DeepSeekProvider::new("k".into());
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
    fn deepseek_default_omits_reasoning_effort_field() {
        let body = deepseek_body_with(ReasoningEffort::Default);
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn deepseek_off_also_omits_field() {
        // DeepSeek interprets omission as "off"; sending an explicit
        // value would be wrong since `"off"` isn't a valid enum value.
        let body = deepseek_body_with(ReasoningEffort::Off);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn deepseek_low_medium_high_all_map_to_high() {
        for eff in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            let body = deepseek_body_with(eff);
            assert_eq!(body["reasoning_effort"], "high", "for {eff:?}");
        }
    }

    #[test]
    fn deepseek_max_maps_to_max() {
        let body = deepseek_body_with(ReasoningEffort::Max);
        assert_eq!(body["reasoning_effort"], "max");
    }

    /// V4 Flash defaults to thinking-on regardless of `reasoning_effort`
    /// omission. `DeepSeekV4Chat` family is the "chat / non-thinking"
    /// semantic, so Default + Off MUST emit the disable knob.
    #[test]
    fn deepseek_v4_chat_default_disables_thinking() {
        let body = deepseek_body_with(ReasoningEffort::Default);
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
    }

    #[test]
    fn deepseek_v4_chat_off_disables_thinking() {
        let body = deepseek_body_with(ReasoningEffort::Off);
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
    }

    /// Any non-default effort leaves thinking up to the model's
    /// `reasoning_effort` handling. Don't send the disable knob.
    #[test]
    fn deepseek_v4_chat_high_omits_thinking_disable() {
        let body = deepseek_body_with(ReasoningEffort::High);
        assert!(body.get("thinking").is_none(), "{body}");
    }

    /// `DeepSeekV4Pro` keeps thinking-on by default (it's the flagship
    /// reasoner-friendly variant), so we don't auto-disable for it.
    #[test]
    fn deepseek_v4_pro_default_omits_thinking_disable() {
        let provider = DeepSeekProvider::new("k".into()).with_family(ModelFamily::DeepSeekV4Pro);
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
            reasoning_effort: ReasoningEffort::Default,
        };
        let body = serde_json::to_value(provider.build_request_body(&req)).expect("serializes");
        assert!(body.get("thinking").is_none(), "{body}");
    }

    /// DeepSeek thinking-mode demands prior assistant turns' reasoning_content
    /// be echoed back. The executor stores it inside the Structured envelope
    /// alongside text + tool_calls; the request builder must lift it to the
    /// message-level `reasoning_content` field.
    #[test]
    fn assistant_structured_envelope_round_trips_reasoning_content() {
        let provider = DeepSeekProvider::new("k".into());
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                ChatMessage {
                    role: Role::User,
                    content: MessageContent::Text("fetch news".into()),
                },
                ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Structured(serde_json::json!({
                        "text": "Calling the web search now.",
                        "tool_calls": [{
                            "id": "call_1",
                            "name": "web_search",
                            "arguments": {"q": "ai news"}
                        }],
                        "reasoning_content": "The user wants a news briefing — start with AI."
                    })),
                },
                ChatMessage {
                    role: Role::Tool,
                    content: MessageContent::Structured(serde_json::json!({
                        "tool_call_id": "call_1",
                        "content": "{\"results\":[]}"
                    })),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::Medium,
        };
        let body = serde_json::to_value(provider.build_request_body(&req)).expect("serializes");
        let messages = body["messages"].as_array().expect("messages");

        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        assert_eq!(
            assistant["reasoning_content"], "The user wants a news briefing — start with AI.",
            "reasoning_content must round-trip on assistant tool-call turn: {assistant}"
        );
        assert_eq!(assistant["content"], "Calling the web search now.");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    }

    /// Assistant turns without reasoning_content must not invent a value — the
    /// field is omitted from the wire so non-thinking turns stay byte-identical.
    #[test]
    fn assistant_structured_envelope_omits_reasoning_when_absent() {
        let provider = DeepSeekProvider::new("k".into());
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::Assistant,
                content: MessageContent::Structured(serde_json::json!({
                    "text": "ok",
                    "tool_calls": [{
                        "id": "call_x",
                        "name": "echo",
                        "arguments": {}
                    }]
                })),
            }],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::Default,
        };
        let body = serde_json::to_value(provider.build_request_body(&req)).expect("serializes");
        let assistant = body["messages"]
            .as_array()
            .and_then(|a| a.iter().find(|m| m["role"] == "assistant"))
            .expect("assistant message");
        assert!(
            assistant.get("reasoning_content").is_none(),
            "expected reasoning_content to be absent: {assistant}"
        );
    }

    /// Cache hits should bill at 10% of the miss rate. A 1M-token call
    /// that is 100% cache hit costs ~10× less than the same call entirely
    /// served from a fresh prefix.
    #[test]
    fn deepseek_cost_discounts_cache_hits() {
        // 1M input tokens, 0 output. No hits: full price.
        let full = estimate_deepseek_cost("deepseek-chat", 1_000_000, 0, 0);
        // Same call, all-cache. Should be exactly 10% of full.
        let all_hit = estimate_deepseek_cost("deepseek-chat", 1_000_000, 0, 1_000_000);
        assert!(
            (full - 0.14).abs() < 1e-9,
            "full-miss baseline expected $0.14, got {full}"
        );
        assert!(
            (all_hit - 0.014).abs() < 1e-9,
            "all-hit expected $0.014, got {all_hit}"
        );
    }

    /// Mixed hit/miss splits should weight the two rates correctly: 50%
    /// hit on 1M tokens = 500k × $0.14/M + 500k × $0.014/M = $0.077.
    #[test]
    fn deepseek_cost_handles_partial_cache_hit() {
        let mixed = estimate_deepseek_cost("deepseek-chat", 1_000_000, 0, 500_000);
        assert!(
            (mixed - 0.077).abs() < 1e-9,
            "50%% hit expected $0.077, got {mixed}"
        );
    }

    /// A bogus `cache_hit > prompt_tokens` value (shouldn't happen but
    /// defending against API quirks) must not produce a negative miss
    /// count. Clamp keeps cost finite.
    #[test]
    fn deepseek_cost_clamps_oversized_cache_hit() {
        let clamped = estimate_deepseek_cost("deepseek-chat", 100, 0, 999_999);
        // All 100 tokens are hits, miss = 0.
        let expected = (100.0 / 1_000_000.0) * 0.014;
        assert!(
            (clamped - expected).abs() < 1e-12,
            "oversized hit should clamp, expected {expected}, got {clamped}"
        );
    }

    /// Response parser plumbs `prompt_cache_hit_tokens` from the wire into
    /// `TokenUsage.cached_tokens` and bills the cost accordingly.
    #[test]
    fn deepseek_response_surfaces_cache_hit_tokens() {
        let raw = r#"{
            "model": "deepseek-chat",
            "choices": [{
                "message": {"content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 10,
                "total_tokens": 1010,
                "prompt_cache_hit_tokens": 800,
                "prompt_cache_miss_tokens": 200
            }
        }"#;
        let parsed: OpenAiResponse = serde_json::from_str(raw).expect("parses");
        let u = parsed.usage.expect("usage present");
        assert_eq!(u.prompt_cache_hit_tokens, Some(800));
        // Cost on 200 miss + 800 hit: 200/M × 0.14 + 800/M × 0.014 = $0.0000392
        let cost =
            estimate_deepseek_cost("deepseek-chat", u.prompt_tokens, u.completion_tokens, 800);
        let expected = (200.0 / 1_000_000.0) * 0.14
            + (800.0 / 1_000_000.0) * 0.014
            + (10.0 / 1_000_000.0) * 0.28;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected {expected}, got {cost}"
        );
    }
}
