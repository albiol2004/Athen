//! Anthropic (Claude) provider adapter.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

        // System prompt rides as a single cacheable text block. The breakpoint
        // on it caches `tools` + `system` together (Anthropic render order is
        // tools → system → messages), so the ~4-6k-token static prefix bills at
        // cache-read rates (~0.1x) on every turn after the first instead of full
        // price. Below the per-model minimum (2048 Sonnet / 4096 Opus) the API
        // silently skips caching — no error.
        let system = request
            .system_prompt
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                vec![AnthropicSystemBlock {
                    block_type: "text",
                    text: s.clone(),
                    cache_control: Some(CacheControl::EPHEMERAL),
                }]
            });

        // Native tool definitions. Without this field Claude cannot emit
        // structured `tool_use` blocks at all — the response parser and the
        // assistant/​tool history round-trip already speak tool_use/tool_result,
        // so this completes the path. `input_schema` is our `parameters` JSON
        // Schema verbatim.
        let tools = request
            .tools
            .as_ref()
            .map(|defs| {
                defs.iter()
                    .map(|td| AnthropicTool {
                        name: td.name.clone(),
                        description: td.description.clone(),
                        input_schema: td.parameters.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        AnthropicRequest {
            model: self.default_model.clone(),
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            system,
            tools,
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

        let cache_read = api_response.usage.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = api_response.usage.cache_creation_input_tokens.unwrap_or(0);
        let usage = TokenUsage {
            prompt_tokens: api_response.usage.input_tokens,
            completion_tokens: api_response.usage.output_tokens,
            // True total includes the cached portion, which Anthropic reports
            // separately from `input_tokens`.
            total_tokens: api_response.usage.input_tokens
                + cache_read
                + cache_creation
                + api_response.usage.output_tokens,
            estimated_cost_usd: Some(estimate_anthropic_cost(
                &api_response.model,
                api_response.usage.input_tokens,
                api_response.usage.output_tokens,
                cache_read,
                cache_creation,
            )),
            cached_tokens: api_response.usage.cache_read_input_tokens,
            cache_creation_tokens: api_response.usage.cache_creation_input_tokens,
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
            .scan(
                (Vec::<u8>::new(), ToolUseAccumulator::default()),
                |(buffer, acc), result| {
                    let emitted: Vec<Result<LlmChunk>> = match result {
                        Ok(bytes) => {
                            buffer.extend_from_slice(&bytes);
                            drain_complete_sse_events(buffer, acc)
                        }
                        Err(e) => vec![Err(AthenError::LlmProvider {
                            provider: "anthropic".into(),
                            message: format!("stream error: {}", e),
                        })],
                    };
                    futures::future::ready(Some(emitted))
                },
            )
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
fn drain_complete_sse_events(
    buffer: &mut Vec<u8>,
    acc: &mut ToolUseAccumulator,
) -> Vec<Result<LlmChunk>> {
    let mut out = Vec::new();
    while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
        let event_bytes: Vec<u8> = buffer.drain(..end).collect();
        buffer.drain(..2); // drop the `\n\n` terminator
        let event_text = String::from_utf8_lossy(&event_bytes);
        out.extend(parse_sse_chunks(&event_text, acc));
    }
    out
}

/// Accumulates a native Claude `tool_use` block across the SSE events that
/// carry it. A single tool call streams as `content_block_start`
/// (`{type:"tool_use", id, name}`), then a run of `content_block_delta`
/// events whose `delta.partial_json` fragments concatenate into the
/// arguments JSON, terminated by `content_block_stop`. The events are keyed
/// by the top-level `index`, so we collect fragments per index and finalize
/// on stop. Mirrors the OpenAI `ToolCallAccumulator` shape.
#[derive(Debug, Default)]
struct ToolUseAccumulator {
    parts: BTreeMap<u64, PartialToolUse>,
}

#[derive(Debug, Default)]
struct PartialToolUse {
    id: String,
    name: String,
    partial_json: String,
}

impl ToolUseAccumulator {
    /// Record a `content_block_start` for a `tool_use` block at `index`.
    fn start(&mut self, index: u64, id: String, name: String) {
        let entry = self.parts.entry(index).or_default();
        entry.id = id;
        entry.name = name;
        entry.partial_json.clear();
    }

    /// Append an `input_json_delta.partial_json` fragment to the block at
    /// `index`. Deltas for content blocks we never saw a tool_use
    /// `content_block_start` for (e.g. text blocks) never reach here.
    fn push_partial(&mut self, index: u64, fragment: &str) {
        if let Some(entry) = self.parts.get_mut(&index) {
            entry.partial_json.push_str(fragment);
        }
    }

    /// Finalize and remove the block at `index`, if present and named.
    fn finalize(&mut self, index: u64) -> Option<ToolCall> {
        self.parts.remove(&index).and_then(finalize_part)
    }

    /// Finalize every remaining block (defensive: a `message_stop` without
    /// a preceding `content_block_stop`).
    fn drain(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.parts)
            .into_values()
            .filter_map(finalize_part)
            .collect()
    }
}

/// Convert an accumulated partial tool_use into a finished [`ToolCall`].
/// Unnamed blocks are dropped (incomplete); empty `partial_json` (a tool
/// that takes no arguments) becomes `{}`. A `partial_json` that fails to
/// parse also falls back to `{}` rather than dropping the call.
fn finalize_part(p: PartialToolUse) -> Option<ToolCall> {
    if p.name.is_empty() {
        return None;
    }
    let arguments = if p.partial_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&p.partial_json)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    };
    Some(ToolCall {
        id: p.id,
        name: p.name,
        arguments,
        thought_signature: None,
    })
}

/// Parse SSE text into LlmChunk results.
///
/// `acc` persists across SSE events (and TCP chunks) so a native Claude
/// `tool_use` block — split across `content_block_start`, many
/// `input_json_delta` deltas, and `content_block_stop` — is reassembled
/// into a single [`ToolCall`]. Emitting a tool call also flags the chunk's
/// `tool_calls` as non-empty, which the streaming wrapper reads to set
/// `saw_structured` and suppress the inline-text extractor.
fn parse_sse_chunks(text: &str, acc: &mut ToolUseAccumulator) -> Vec<Result<LlmChunk>> {
    let mut chunks = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                chunks.push(Ok(LlmChunk {
                    delta: String::new(),
                    is_final: true,
                    is_thinking: false,
                    tool_calls: acc.drain(),
                }));
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(data) {
                Ok(event) => {
                    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);

                    match event_type {
                        "content_block_start" => {
                            // Capture the id+name of a native tool_use block so
                            // its forthcoming input_json_delta fragments can be
                            // keyed and accumulated. Text/thinking blocks are
                            // ignored here.
                            let block = event.get("content_block");
                            let is_tool_use = block
                                .and_then(|b| b.get("type"))
                                .and_then(|t| t.as_str())
                                == Some("tool_use");
                            if is_tool_use {
                                let id = block
                                    .and_then(|b| b.get("id"))
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .and_then(|b| b.get("name"))
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                acc.start(index, id, name);
                            }
                        }
                        "content_block_delta" => {
                            let delta = event.get("delta");
                            let delta_type = delta
                                .and_then(|d| d.get("type"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            match delta_type {
                                // Tool-call arguments arrive as a stream of
                                // partial_json fragments to concatenate.
                                "input_json_delta" => {
                                    if let Some(frag) = delta
                                        .and_then(|d| d.get("partial_json"))
                                        .and_then(|p| p.as_str())
                                    {
                                        acc.push_partial(index, frag);
                                    }
                                }
                                // Plain assistant text.
                                _ => {
                                    if let Some(text_delta) = delta
                                        .and_then(|d| d.get("text"))
                                        .and_then(|t| t.as_str())
                                    {
                                        chunks.push(Ok(LlmChunk {
                                            delta: text_delta.to_string(),
                                            is_final: false,
                                            is_thinking: false,
                                            tool_calls: vec![],
                                        }));
                                    }
                                }
                            }
                        }
                        "content_block_stop" => {
                            // Finalize a completed tool_use block (no-op for
                            // text/thinking blocks — they aren't in the
                            // accumulator). Emit it as a non-text chunk so the
                            // wrapper sets `saw_structured`.
                            if let Some(call) = acc.finalize(index) {
                                chunks.push(Ok(LlmChunk {
                                    delta: String::new(),
                                    is_final: false,
                                    is_thinking: false,
                                    tool_calls: vec![call],
                                }));
                            }
                        }
                        "message_stop" => {
                            chunks.push(Ok(LlmChunk {
                                delta: String::new(),
                                is_final: true,
                                is_thinking: false,
                                // Defensive: flush any block that never saw a
                                // content_block_stop.
                                tool_calls: acc.drain(),
                            }));
                        }
                        _ => {
                            // Ignore other event types (message_start,
                            // message_delta, ping, etc.)
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
fn estimate_anthropic_cost(
    model: &str,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
) -> f64 {
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
    // Cache reads bill at ~0.1x the input rate; 5-minute cache writes at ~1.25x.
    let cache_read_cost = (cache_read_tokens as f64 / 1_000_000.0) * input_per_m * 0.1;
    let cache_write_cost = (cache_creation_tokens as f64 / 1_000_000.0) * input_per_m * 1.25;
    input_cost + output_cost + cache_read_cost + cache_write_cost
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
    system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

/// A `system` content block. Anthropic accepts `system` as a bare string or an
/// array of typed blocks; the array form is required to attach `cache_control`.
#[derive(Debug, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// A native tool definition. `input_schema` is the JSON Schema for the tool's
/// arguments (our `ToolDefinition.parameters`).
#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// `cache_control: {"type": "ephemeral"}` marker (5-minute TTL). Placed on the
/// last system block to cache the tools+system prefix; max 4 such breakpoints
/// per request (we use one).
#[derive(Debug, Serialize, Clone, Copy)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: &'static str,
}

impl CacheControl {
    const EPHEMERAL: CacheControl = CacheControl {
        cache_type: "ephemeral",
    };
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
    /// Tokens served from the prompt cache (~0.1x input price). Present only
    /// when a `cache_control` breakpoint hit on this request. Note Anthropic's
    /// `input_tokens` is the *uncached* remainder — true prompt size is
    /// `input + cache_read + cache_creation`.
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    /// Tokens written to the prompt cache this request (~1.25x input price).
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
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

    /// Build a request body value with the given tools + system prompt.
    fn body_with(
        tools: Option<Vec<athen_core::tool::ToolDefinition>>,
        system_prompt: Option<String>,
    ) -> serde_json::Value {
        let provider = AnthropicProvider::new("k".into(), "claude-sonnet-4-6".into());
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
            }],
            max_tokens: None,
            temperature: None,
            tools,
            system_prompt,
            reasoning_effort: ReasoningEffort::default(),
        };
        serde_json::to_value(provider.build_request_body(&req)).expect("serializes")
    }

    fn sample_tool() -> athen_core::tool::ToolDefinition {
        athen_core::tool::ToolDefinition {
            name: "get_weather".into(),
            description: "Get weather".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            backend: athen_core::tool::ToolBackend::Shell {
                command: "echo".into(),
                native: true,
            },
            base_risk: athen_core::risk::BaseImpact::Read,
        }
    }

    #[test]
    fn tools_serialize_with_input_schema() {
        let body = body_with(Some(vec![sample_tool()]), Some("You are helpful.".into()));
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Get weather");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn system_prompt_is_cacheable_block() {
        let body = body_with(None, Some("static prefix".into()));
        let sys = body["system"].as_array().expect("system as array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "static prefix");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn empty_tools_and_system_are_omitted() {
        let body = body_with(Some(vec![]), Some(String::new()));
        assert!(body.get("tools").is_none(), "empty tools omitted: {body}");
        assert!(body.get("system").is_none(), "empty system omitted: {body}");
        // None likewise omits both.
        let body = body_with(None, None);
        assert!(body.get("tools").is_none());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn usage_parses_cache_tokens() {
        let json = serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 4000,
            "cache_creation_input_tokens": 0
        });
        let usage: AnthropicUsage = serde_json::from_value(json).unwrap();
        assert_eq!(usage.cache_read_input_tokens, Some(4000));
        assert_eq!(usage.cache_creation_input_tokens, Some(0));

        // Backwards-compat: absent cache fields default to None (don't 400 on
        // responses that predate caching).
        let bare = serde_json::json!({"input_tokens": 10, "output_tokens": 5});
        let usage: AnthropicUsage = serde_json::from_value(bare).unwrap();
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    /// A representative Anthropic streaming sequence for a turn that emits a
    /// sentence of prose AND a native `tool_use` block: text delta →
    /// content_block_start(tool_use) → input_json_delta fragments →
    /// content_block_stop → message_stop. Feeds it through the same
    /// `drain_complete_sse_events` path the live stream uses (events split on
    /// `\n\n`) and asserts both the text and a fully-parsed tool call survive.
    #[test]
    fn streaming_parses_native_tool_use_alongside_text() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me read that file.\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"read_file\",\"input\":{}}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp/x.txt\\\"}\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut buffer = sse.as_bytes().to_vec();
        let mut acc = ToolUseAccumulator::default();
        let chunks: Vec<LlmChunk> = drain_complete_sse_events(&mut buffer, &mut acc)
            .into_iter()
            .map(|r| r.expect("chunk ok"))
            .collect();

        // Text delta survived.
        let text: String = chunks.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(text, "Let me read that file.");

        // Exactly one tool call, with id, name, and parsed args.
        let calls: Vec<&ToolCall> = chunks.iter().flat_map(|c| c.tool_calls.iter()).collect();
        assert_eq!(calls.len(), 1, "expected one tool call, got {:?}", calls);
        assert_eq!(calls[0].id, "toolu_abc");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "/tmp/x.txt");

        // A chunk carrying a tool call is what the wrapper reads to set
        // `saw_structured`, suppressing the inline-text extractor.
        assert!(
            chunks.iter().any(|c| !c.tool_calls.is_empty()),
            "a chunk must carry the structured tool call"
        );

        // Final chunk is the terminator.
        assert!(chunks.last().expect("a chunk").is_final);
    }

    /// A tool_use block with no arguments (empty input) finalizes to `{}`
    /// rather than dropping the call.
    #[test]
    fn streaming_tool_use_with_no_args_becomes_empty_object() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_z\",\"name\":\"get_time\",\"input\":{}}}\n",
            "\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );
        let mut buffer = sse.as_bytes().to_vec();
        let mut acc = ToolUseAccumulator::default();
        let chunks: Vec<LlmChunk> = drain_complete_sse_events(&mut buffer, &mut acc)
            .into_iter()
            .map(|r| r.expect("chunk ok"))
            .collect();
        let calls: Vec<&ToolCall> = chunks.iter().flat_map(|c| c.tool_calls.iter()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_time");
        assert!(calls[0].arguments.is_object());
        assert_eq!(calls[0].arguments.as_object().unwrap().len(), 0);
    }

    #[test]
    fn cost_discounts_cache_reads() {
        // 1M cached read tokens on sonnet ($3/M input) bills at 0.1x = $0.30.
        let cost = estimate_anthropic_cost("claude-sonnet-4-6", 0, 0, 1_000_000, 0);
        assert!((cost - 0.30).abs() < 1e-9, "cache read cost: {cost}");
        // 1M cache-write tokens bills at 1.25x = $3.75.
        let cost = estimate_anthropic_cost("claude-sonnet-4-6", 0, 0, 0, 1_000_000);
        assert!((cost - 3.75).abs() < 1e-9, "cache write cost: {cost}");
    }
}
