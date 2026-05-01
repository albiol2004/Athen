//! Generic OpenAI-compatible provider adapter.
//!
//! Works with any server that exposes the OpenAI chat completions API:
//! OpenAI itself, Ollama, llama.cpp, LM Studio, vLLM, text-generation-webui,
//! and any other compatible endpoint.

use std::collections::BTreeMap;

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
                    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
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

        // Stateful SSE parser: a `ToolCallAccumulator` is threaded across
        // every byte chunk so fragmented tool-call deltas (id+name in one
        // event, arguments dribbled across many later events) are merged
        // into a single emitted ToolCall when finish_reason / [DONE] arrives.
        //
        // `pending_bytes` buffers raw bytes (NOT decoded text) across chunks
        // so SSE event lines split across HTTP byte chunks reassemble
        // correctly, AND multi-byte UTF-8 codepoints split at chunk seams
        // are not corrupted by `from_utf8_lossy`.
        let chunk_stream = futures::stream::unfold(
            StreamState {
                byte_stream,
                acc: ToolCallAccumulator::default(),
                pending_bytes: Vec::new(),
                provider_id,
                done: false,
            },
            |mut state| async move {
                if state.done {
                    return None;
                }
                let parsed = match state.byte_stream.next().await {
                    Some(Ok(bytes)) => {
                        state.pending_bytes.extend_from_slice(&bytes);
                        let complete = take_complete_lines(&mut state.pending_bytes);
                        if complete.is_empty() {
                            Vec::new()
                        } else {
                            let text = String::from_utf8_lossy(&complete);
                            parse_sse_chunks(&text, &state.provider_id, &mut state.acc)
                        }
                    }
                    Some(Err(e)) => vec![Err(AthenError::LlmProvider {
                        provider: state.provider_id.clone(),
                        message: format!("stream error: {}", e),
                    })],
                    None => {
                        // End of byte stream. Flush any final partial line
                        // and drain the accumulator so callers still receive
                        // tool calls if the server omitted [DONE].
                        state.done = true;
                        let mut out = Vec::new();
                        if !state.pending_bytes.is_empty() {
                            let tail = std::mem::take(&mut state.pending_bytes);
                            let text = String::from_utf8_lossy(&tail);
                            out.extend(parse_sse_chunks(&text, &state.provider_id, &mut state.acc));
                        }
                        if !state.acc.is_empty() {
                            let tool_calls = state.acc.drain();
                            if !tool_calls.is_empty() {
                                out.push(Ok(LlmChunk {
                                    delta: String::new(),
                                    is_final: true,
                                    is_thinking: false,
                                    tool_calls,
                                }));
                            }
                        }
                        out
                    }
                };
                Some((futures::stream::iter(parsed), state))
            },
        )
        .flatten();

        Ok(Box::pin(chunk_stream))
    }

    async fn is_available(&self) -> bool {
        // Cloud providers with an API key are assumed available.
        // Override this for local providers that need a health check.
        self.api_key.is_some()
    }
}

/// Per-stream state for the SSE `unfold` loop.
///
/// Generic over the byte-stream type so both this provider and DeepSeek
/// (which reuses the same buffering logic) can share it without naming
/// `reqwest`'s private `Bytes` stream type explicitly.
struct StreamState<S> {
    byte_stream: S,
    acc: ToolCallAccumulator,
    pending_bytes: Vec<u8>,
    provider_id: String,
    done: bool,
}

/// Split off the longest prefix of `buf` that ends in `\n`, returning it.
///
/// The trailing partial line (everything after the last `\n`) stays in
/// `buf` for the next iteration. Operates on raw bytes so multi-byte
/// UTF-8 codepoints split across HTTP byte chunks are never decoded
/// into U+FFFD replacement characters.
pub(crate) fn take_complete_lines(buf: &mut Vec<u8>) -> Vec<u8> {
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(idx) => {
            let rest = buf.split_off(idx + 1);
            std::mem::replace(buf, rest)
        }
        None => Vec::new(),
    }
}

/// Parse tool-call arguments from the raw JSON string an OpenAI-shaped
/// provider returns, repairing common quirks before falling back.
///
/// Tries strict `serde_json::from_str` first. When that fails (the
/// known DeepSeek quirk of embedding raw control characters like
/// literal newlines/tabs inside string values when the value is large
/// HTML/code), retries with a lenient pass that re-escapes unescaped
/// control characters inside string literals only. As a last resort
/// the raw string is wrapped in `Value::String` so the executor still
/// sees something — `do_*` helpers in `athen-agent::tools` then run a
/// second-line `coerce_args` pass before extracting fields.
pub(crate) fn parse_tool_arguments(raw: &str) -> serde_json::Value {
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    let control_repaired = escape_control_chars_in_strings(raw);
    if let Ok(v) = serde_json::from_str(&control_repaired) {
        tracing::warn!(
            "Tool args contained unescaped control characters; \
             repaired and parsed (raw len={})",
            raw.len()
        );
        return v;
    }
    // Last resort: also escape unescaped double-quotes inside string
    // literals. Triggered by HTML/code content where the LLM forgets to
    // escape attribute quotes (e.g. `class="hero"`). Heuristic: a `"`
    // is a real string terminator iff the next non-whitespace char is
    // `,`, `}`, `]`, or end-of-input. Otherwise treat it as a literal
    // quote and emit `\"` instead.
    let aggressive = escape_unescaped_quotes_in_strings(&control_repaired);
    if let Ok(v) = serde_json::from_str(&aggressive) {
        tracing::warn!(
            "Tool args contained unescaped quotes inside string values; \
             aggressive repair succeeded (raw len={})",
            raw.len()
        );
        return v;
    }
    tracing::warn!(
        "Tool args could not be parsed even after aggressive repair; \
         falling back to string wrapper (raw len={})",
        raw.len()
    );
    serde_json::Value::String(raw.to_string())
}

/// Walk the input character by character, tracking whether we're
/// inside a JSON string literal, and escape unescaped control chars
/// (`\n`, `\r`, `\t`, etc.) into their proper escape sequences.
/// Anything outside string literals is left alone (so the JSON
/// structure itself is preserved).
fn escape_control_chars_in_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut in_string = false;
    let mut prev_backslash = false;
    for ch in input.chars() {
        if in_string {
            if prev_backslash {
                out.push(ch);
                prev_backslash = false;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    prev_backslash = true;
                }
                '"' => {
                    out.push(ch);
                    in_string = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Aggressive repair: walk the input and escape any unescaped `"` inside
/// string literals. Heuristic for "is this `"` the real string end?" —
/// peek ahead past whitespace; if the next char is `,`, `}`, `]`, `:`,
/// or end-of-input, treat as terminator; otherwise escape it and stay
/// in-string. This is what saves us when the LLM stuffs HTML or code
/// (like `class="hero"`) into a string argument without escaping.
///
/// Imperfect — content like `"foo "bar","baz"` would be mis-parsed —
/// but the failure mode here is "fall back to Value::String", same as
/// before, so it's strictly better than not trying.
fn escape_unescaped_quotes_in_strings(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 16);
    let mut i = 0;
    let mut in_string = false;
    let mut prev_backslash = false;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            if prev_backslash {
                out.push(ch);
                prev_backslash = false;
                i += 1;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    prev_backslash = true;
                }
                '"' => {
                    // Look ahead past whitespace for a terminator-like char.
                    let mut j = i + 1;
                    while j < chars.len() && chars[j].is_whitespace() {
                        j += 1;
                    }
                    let is_terminator = j >= chars.len()
                        || matches!(chars[j], ',' | '}' | ']' | ':');
                    if is_terminator {
                        out.push('"');
                        in_string = false;
                    } else {
                        out.push('\\');
                        out.push('"');
                    }
                }
                _ => out.push(ch),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
        i += 1;
    }
    out
}

/// Per-tool-call buffer used while assembling fragmented streaming deltas.
#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments_buf: String,
}

/// Stateful accumulator for OpenAI-compatible streaming tool calls.
///
/// In OpenAI's SSE format, a single tool call is split across many delta
/// events keyed by `index`: the first event typically carries the `id` and
/// `name` (with empty arguments); subsequent events carry only `arguments`
/// fragments that must be concatenated. The accumulator collects these
/// fragments per-index and finalizes them into [`ToolCall`]s on demand.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    parts: BTreeMap<u32, PartialToolCall>,
}

impl ToolCallAccumulator {
    /// Ingest a single `tool_calls[i]` JSON value from a streaming delta.
    fn ingest(&mut self, tc_val: &serde_json::Value) {
        let index = tc_val
            .get("index")
            .and_then(|v| v.as_u64())
            .map(|i| i as u32)
            .unwrap_or(0);
        let entry = self.parts.entry(index).or_default();

        if let Some(id) = tc_val.get("id").and_then(|v| v.as_str()) {
            if entry.id.is_none() && !id.is_empty() {
                entry.id = Some(id.to_string());
            }
        }
        if let Some(name) = tc_val
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            if entry.name.is_none() && !name.is_empty() {
                entry.name = Some(name.to_string());
            }
        }
        if let Some(args_frag) = tc_val
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
        {
            entry.arguments_buf.push_str(args_frag);
        }
    }

    /// Drain all accumulated entries into finalized [`ToolCall`]s.
    ///
    /// Entries lacking a `name` are discarded (incomplete). Missing `id`s
    /// are synthesized from the index. Empty argument buffers become an
    /// empty JSON object (some tools take no arguments).
    pub(crate) fn drain(&mut self) -> Vec<ToolCall> {
        let parts = std::mem::take(&mut self.parts);
        parts
            .into_iter()
            .filter_map(|(index, p)| {
                let name = p.name?;
                let id = p.id.unwrap_or_else(|| format!("call_{}", index));
                let arguments = if p.arguments_buf.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    parse_tool_arguments(&p.arguments_buf)
                };
                Some(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// Parse SSE text into `LlmChunk` results (OpenAI streaming format).
///
/// `acc` must persist across byte-stream chunks so that tool-call deltas
/// fragmented across multiple SSE events are merged correctly.
///
/// This is public so wrapper providers (e.g. DeepSeek) can reuse it.
pub fn parse_sse_chunks(
    text: &str,
    provider_id: &str,
    acc: &mut ToolCallAccumulator,
) -> Vec<Result<LlmChunk>> {
    let mut chunks = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        debug!(provider = provider_id, raw_sse = data, "SSE chunk received");

        if data == "[DONE]" {
            let tool_calls = acc.drain();
            chunks.push(Ok(LlmChunk {
                delta: String::new(),
                is_final: true,
                is_thinking: false,
                tool_calls,
            }));
            continue;
        }

        let event = match serde_json::from_str::<serde_json::Value>(data) {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    provider = provider_id,
                    data = data,
                    "failed to parse SSE event data"
                );
                continue;
            }
        };

        let delta_obj = event
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"));

        // Reasoning / thinking content (Qwen 3.5, DeepSeek R1, ...).
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

        // Plain text content delta.
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

        // Tool-call fragments — accumulate, do not emit yet.
        if let Some(tool_calls_arr) = delta_obj
            .and_then(|d| d.get("tool_calls"))
            .and_then(|tc| tc.as_array())
        {
            for tc_val in tool_calls_arr {
                acc.ingest(tc_val);
            }
        }

        // finish_reason marks the end of the message: drain accumulator
        // and emit a single final chunk that carries all assembled tool calls.
        if let Some(finish) = event
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
        {
            if finish == "stop" || finish == "length" || finish == "tool_calls" {
                let tool_calls = acc.drain();
                chunks.push(Ok(LlmChunk {
                    delta: String::new(),
                    is_final: true,
                    is_thinking: false,
                    tool_calls,
                }));
            }
        }
    }

    // If the upstream stream ends without a finish_reason or [DONE]
    // (rare, but seen with some compatibility servers), the caller should
    // still get the accumulated tool calls. We don't flush here because
    // we're called per byte chunk, not at end-of-stream — a later chunk
    // may extend the buffer. Anything left in `acc` will be flushed when
    // the next finish_reason / [DONE] arrives, or dropped on stream end.
    let _ = acc.is_empty();

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

    #[test]
    fn aggressive_repair_escapes_unescaped_html_quotes() {
        // The exact failure mode from the field: HTML stuffed into the
        // `content` arg of a `write` call, with `class="hero"` style
        // attributes whose quotes are not JSON-escaped.
        let raw = r#"{"path":"index.html","content":"<div class="hero">Hi</div>"}"#;
        // Strict + control-char repair both fail.
        assert!(serde_json::from_str::<serde_json::Value>(raw).is_err());
        let v = parse_tool_arguments(raw);
        // Aggressive repair recovered an object.
        assert!(v.is_object(), "expected object, got {v:?}");
        assert_eq!(v.get("path").and_then(|x| x.as_str()), Some("index.html"));
        assert!(v.get("content").and_then(|x| x.as_str()).unwrap().contains("class="));
    }

    #[test]
    fn aggressive_repair_handles_combined_quotes_and_newlines() {
        // Both failure modes at once: raw newlines AND unescaped quotes.
        let raw = "{\"path\":\"x.html\",\"content\":\"<a href=\"#\">\nlink</a>\"}";
        let v = parse_tool_arguments(raw);
        assert!(v.is_object(), "expected object, got {v:?}");
        let content = v.get("content").and_then(|x| x.as_str()).unwrap();
        assert!(content.contains("href="));
        assert!(content.contains("link"));
    }

    #[test]
    fn aggressive_repair_leaves_clean_json_alone() {
        // Sanity: well-formed JSON shouldn't be touched.
        let raw = r#"{"a":"hello","b":"world"}"#;
        let v = parse_tool_arguments(raw);
        assert_eq!(v.get("a").and_then(|x| x.as_str()), Some("hello"));
        assert_eq!(v.get("b").and_then(|x| x.as_str()), Some("world"));
    }

    #[test]
    fn aggressive_repair_unparseable_falls_back_to_string_wrapper() {
        // Truly broken input still wraps to Value::String — no regression.
        let raw = "this is not json at all { ;;;";
        let v = parse_tool_arguments(raw);
        assert!(v.is_string());
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
        assert_eq!(body.messages[0].tool_call_id, Some("call_123".to_string()));
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
        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].as_ref().unwrap().delta, "Hello");
        assert!(!chunks[0].as_ref().unwrap().is_final);
        assert_eq!(chunks[1].as_ref().unwrap().delta, " world");
        assert!(chunks[2].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_parse_sse_chunks_finish_reason() {
        let sse = r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#;
        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_parse_sse_ignores_non_data_lines() {
        let sse = ": this is a comment\nsome random line\ndata: [DONE]\n";
        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].as_ref().unwrap().is_final);
    }

    #[test]
    fn test_parse_sse_chunks_fragmented_tool_call() {
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"files__list_dir\",\"arguments\":\"\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"pa\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"/h\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ome\\\"}\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n";

        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);

        let final_chunks: Vec<&LlmChunk> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter(|c| c.is_final)
            .collect();
        assert_eq!(final_chunks.len(), 1);
        assert_eq!(final_chunks[0].tool_calls.len(), 1);

        let tc = &final_chunks[0].tool_calls[0];
        assert_eq!(tc.id, "call_abc");
        assert_eq!(tc.name, "files__list_dir");
        assert_eq!(tc.arguments, serde_json::json!({"path": "/home"}));

        let mid_tool_chunks: Vec<&LlmChunk> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter(|c| !c.is_final && !c.tool_calls.is_empty())
            .collect();
        assert!(
            mid_tool_chunks.is_empty(),
            "fragments must not be emitted before finish_reason"
        );
    }

    #[test]
    fn test_parse_sse_chunks_multiple_parallel_tool_calls() {
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"foo\",\"arguments\":\"\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"bar\",\"arguments\":\"\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"y\\\":2}\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n";

        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);

        let final_chunk = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .find(|c| c.is_final)
            .expect("final chunk");
        assert_eq!(final_chunk.tool_calls.len(), 2);

        let foo = final_chunk
            .tool_calls
            .iter()
            .find(|t| t.name == "foo")
            .expect("foo tool call");
        assert_eq!(foo.id, "call_a");
        assert_eq!(foo.arguments, serde_json::json!({"x": 1}));

        let bar = final_chunk
            .tool_calls
            .iter()
            .find(|t| t.name == "bar")
            .expect("bar tool call");
        assert_eq!(bar.id, "call_b");
        assert_eq!(bar.arguments, serde_json::json!({"y": 2}));
    }

    #[test]
    fn test_parse_sse_chunks_tool_call_no_args() {
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"now\",\"arguments\":\"\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n";

        let mut acc = ToolCallAccumulator::default();
        let chunks = parse_sse_chunks(sse, "test", &mut acc);

        let final_chunk = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .find(|c| c.is_final)
            .expect("final chunk");
        assert_eq!(final_chunk.tool_calls.len(), 1);
        let tc = &final_chunk.tool_calls[0];
        assert_eq!(tc.name, "now");
        assert_eq!(tc.arguments, serde_json::json!({}));
    }

    #[test]
    fn test_parse_sse_chunks_state_persists_across_calls() {
        let mut acc = ToolCallAccumulator::default();

        let part1 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n";
        let part2 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"42}\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n";

        let chunks1 = parse_sse_chunks(part1, "test", &mut acc);
        assert!(chunks1
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .all(|c| !c.is_final));

        let chunks2 = parse_sse_chunks(part2, "test", &mut acc);
        let final_chunk = chunks2
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .find(|c| c.is_final)
            .expect("final chunk");
        assert_eq!(final_chunk.tool_calls.len(), 1);
        assert_eq!(
            final_chunk.tool_calls[0].arguments,
            serde_json::json!({"a": 42})
        );
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
        let provider = OpenAiCompatibleProvider::new("http://localhost:8080".into());
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
    fn test_take_complete_lines_no_newline() {
        let mut buf = b"data: {\"partial\":".to_vec();
        let complete = take_complete_lines(&mut buf);
        assert!(complete.is_empty());
        assert_eq!(buf, b"data: {\"partial\":");
    }

    #[test]
    fn test_take_complete_lines_splits_at_last_newline() {
        let mut buf = b"data: a\ndata: b\ndata: par".to_vec();
        let complete = take_complete_lines(&mut buf);
        assert_eq!(complete, b"data: a\ndata: b\n");
        assert_eq!(buf, b"data: par");
    }

    #[test]
    fn test_take_complete_lines_all_complete() {
        let mut buf = b"data: a\ndata: b\n".to_vec();
        let complete = take_complete_lines(&mut buf);
        assert_eq!(complete, b"data: a\ndata: b\n");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_buffer_holds_partial_utf8() {
        // The emoji 📄 is 0xF0 0x9F 0x93 0x84 in UTF-8.
        // Simulate two byte chunks: the first ends mid-codepoint with no
        // newline so nothing is decoded yet; the second supplies the rest
        // plus a newline.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"data: \"\xF0\x9F"); // first 2 emoji bytes
        let complete1 = take_complete_lines(&mut buf);
        assert!(
            complete1.is_empty(),
            "no newline yet -> no decode, no replacement chars introduced"
        );

        buf.extend_from_slice(b"\x93\x84\"\n"); // remaining 2 bytes + close quote + \n
        let complete2 = take_complete_lines(&mut buf);
        let text = String::from_utf8_lossy(&complete2);
        assert!(text.contains('\u{1F4C4}'), "got: {:?}", text);
        assert!(!text.contains('\u{FFFD}'), "no replacement char");
    }

    #[test]
    fn test_streaming_handles_split_sse_event() {
        // Simulate two byte chunks: chunk 1 ends mid-JSON; chunk 2 completes it.
        let chunk1 = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel";
        let chunk2 = b"lo\"}}]}\n\ndata: [DONE]\n";

        let mut buf: Vec<u8> = Vec::new();
        let mut acc = ToolCallAccumulator::default();
        let mut all_chunks: Vec<Result<LlmChunk>> = Vec::new();

        buf.extend_from_slice(chunk1);
        let complete = take_complete_lines(&mut buf);
        // First chunk has no newline -> nothing complete yet.
        assert!(complete.is_empty());

        buf.extend_from_slice(chunk2);
        let complete = take_complete_lines(&mut buf);
        let text = String::from_utf8_lossy(&complete);
        all_chunks.extend(parse_sse_chunks(&text, "test", &mut acc));

        let content: String = all_chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter(|c| !c.is_final)
            .map(|c| c.delta.clone())
            .collect();
        assert_eq!(content, "Hello");

        assert!(all_chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .any(|c| c.is_final));
    }

    #[test]
    fn test_streaming_split_utf8_codepoint() {
        // Full SSE event: data: {"choices":[{"delta":{"content":"📄"}}]}\n\ndata: [DONE]\n
        // The emoji is 4 bytes: F0 9F 93 84. Split between bytes 2 and 3.
        let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xF0\x9F\x93\x84\"}}]}\n\ndata: [DONE]\n";
        let split_at = full.iter().position(|&b| b == 0xF0).unwrap() + 2;
        let chunk1 = &full[..split_at];
        let chunk2 = &full[split_at..];

        let mut buf: Vec<u8> = Vec::new();
        let mut acc = ToolCallAccumulator::default();
        let mut all_chunks: Vec<Result<LlmChunk>> = Vec::new();

        buf.extend_from_slice(chunk1);
        let complete = take_complete_lines(&mut buf);
        // No newline in chunk1 -> nothing decoded yet (this is what saves us
        // from from_utf8_lossy producing a replacement character at the seam).
        assert!(complete.is_empty());

        buf.extend_from_slice(chunk2);
        let complete = take_complete_lines(&mut buf);
        let text = String::from_utf8_lossy(&complete);
        all_chunks.extend(parse_sse_chunks(&text, "test", &mut acc));

        let content: String = all_chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter(|c| !c.is_final)
            .map(|c| c.delta.clone())
            .collect();
        assert_eq!(content, "\u{1F4C4}");
        assert!(!content.contains('\u{FFFD}'));
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
