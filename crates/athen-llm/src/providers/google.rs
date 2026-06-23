//! Google (Gemini) provider adapter.
//!
//! Talks to the Generative Language API (`generativelanguage.googleapis.com`)
//! using the v1beta `generateContent` / `streamGenerateContent` endpoints.
//! Supports chat, streaming SSE, native function calling, vision via inline
//! base64 images, PDF documents through the same `inlineData` mechanism, and
//! thinking-mode reasoning when the configured family is a Gemini reasoning
//! family (`Gemini3Pro` today).
//!
//! ## Out of scope
//!
//! - File API uploads (large PDFs). We only inline base64 payloads — anything
//!   bigger than a single `inlineData` part is the caller's problem for now.
//! - URL-source images. Gemini doesn't accept arbitrary URLs through
//!   `inlineData`; we warn and skip rather than silently dropping them. The
//!   Gemini `fileData` path is a future feature.
//! ## SSE buffering
//!
//! Gemini's streaming endpoint emits one SSE event per content chunk and a
//! single `functionCall` part carries the WHOLE argument blob inline. A
//! `write` call with a long HTML payload routinely overflows a single TCP
//! segment, so `bytes_stream` hands us the event in two pieces. We accumulate
//! bytes across chunks in a `scan`-style buffer and only feed complete events
//! (terminated by `\n\n`) into the per-event parser. Without this every long
//! tool call silently drops, taking its `thoughtSignature` with it and
//! breaking the next turn with HTTP 400.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::llm::*;
use athen_core::traits::llm::LlmProvider;

use crate::quirks::{self, seed, ModelQuirks, ReasoningSurface};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Google Gemini LLM provider.
pub struct GoogleProvider {
    api_key: String,
    default_model: String,
    client: Client,
    base_url: String,
    supports_vision: bool,
    supports_documents: bool,
    /// Quirks profile resolved from the user-selected `ModelFamily`.
    /// Drives the post-parse pipeline (`apply_to_response`) — Gemini's
    /// thinking surface is native typed parts (`part.thought == true`)
    /// so most of this is a no-op once we lift the right fields out
    /// of the wire response.
    quirks: ModelQuirks,
}

impl GoogleProvider {
    /// Create a new Google provider.
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
            quirks: ModelQuirks::default(),
        }
    }

    /// Set the model family. Picks the quirks profile used to post-process
    /// responses (reasoning extraction etc.) and tells `build_generation_config`
    /// whether to opt the request into thinking mode.
    pub fn with_family(mut self, family: ModelFamily) -> Self {
        self.quirks = seed::quirks_for_family(family);
        self
    }

    /// Mark the configured `default_model` as vision-capable. Gemini 2.5 +
    /// 3.x all accept image inputs natively; older 1.0 text-only models do
    /// not. The caller owns matching this to the actual model slug.
    pub fn with_vision(mut self, supported: bool) -> Self {
        self.supports_vision = supported;
        self
    }

    /// Mark the configured `default_model` as PDF-capable. Gemini accepts
    /// `application/pdf` through the same `inlineData` channel as images.
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

    /// Map our `LlmRequest` messages to Gemini's `contents` array plus an
    /// optional top-level `systemInstruction`. Handles:
    ///
    /// - lifting every `Role::System` into the `systemInstruction` field
    ///   (Gemini only accepts `user` / `model` roles inside `contents`);
    /// - multimodal text+image+PDF payloads as `text` + `inlineData` parts;
    /// - tool-result messages (Role::Tool) wrapped as `functionResponse` parts;
    /// - strict alternation: consecutive same-role turns are merged into a
    ///   single content with concatenated parts.
    fn build_contents(
        &self,
        request: &LlmRequest,
    ) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
        // Collect every system message verbatim, joined with blank lines.
        let mut system_chunks: Vec<String> = Vec::new();
        if let Some(s) = request.system_prompt.as_ref() {
            if !s.is_empty() {
                system_chunks.push(s.clone());
            }
        }
        for m in &request.messages {
            if m.role == Role::System {
                if let MessageContent::Text(t) = &m.content {
                    if !t.is_empty() {
                        system_chunks.push(t.clone());
                    }
                }
            }
        }
        let system_instruction = if system_chunks.is_empty() {
            None
        } else {
            Some(GeminiSystemInstruction {
                role: Some("user".into()),
                parts: vec![GeminiPart::text(system_chunks.join("\n\n"))],
            })
        };

        // Build the contents stream, merging consecutive same-role turns.
        let mut contents: Vec<GeminiContent> = Vec::new();
        for m in &request.messages {
            if m.role == Role::System {
                continue;
            }
            let role = match m.role {
                Role::Assistant => "model",
                // Tool results ride in as a `user` turn carrying a
                // `functionResponse` part — Gemini has no dedicated tool role.
                Role::User | Role::Tool => "user",
                Role::System => unreachable!("filtered above"),
            };
            let parts = match (&m.role, &m.content) {
                // Tool-result turn: lift whatever structured value we have
                // into a `functionResponse` part. The executor's exact shape
                // varies — we accept either a pre-shaped `{name, response, id?}`
                // blob or any other JSON value as the `response` payload.
                (Role::Tool, MessageContent::Structured(v)) => {
                    vec![gemini_function_response_from_value(v)]
                }
                (Role::Tool, MessageContent::Text(t)) => {
                    vec![GeminiPart::function_response(
                        "tool".into(),
                        None,
                        serde_json::json!({ "result": t }),
                    )]
                }
                // Assistant turn that carries a structured tool_call payload
                // (executor round-trip: we emitted these in a prior response
                // and the agent loop fed them back). Translate each into a
                // `functionCall` part so the model can see its own history.
                (Role::Assistant, MessageContent::Structured(v))
                    if v.get("tool_calls").is_some() =>
                {
                    let mut out = Vec::new();
                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            out.push(GeminiPart::text(text.to_string()));
                        }
                    }
                    if let Some(calls) = v.get("tool_calls").and_then(|tc| tc.as_array()) {
                        for c in calls {
                            let name = c
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let id = c.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
                            let args = c
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            // Gemini 3 + thinking-mode 2.5 require us to echo
                            // back the original `thoughtSignature` for any
                            // functionCall part we reproduce from history.
                            // The executor stashes it on `ToolCall`, which
                            // serialises as `thought_signature` here.
                            let sig = c
                                .get("thought_signature")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                            out.push(GeminiPart::function_call(name, id, args, sig));
                        }
                    }
                    out
                }
                (_, MessageContent::Text(t)) => vec![GeminiPart::text(t.clone())],
                (_, MessageContent::Structured(v)) => {
                    // Unknown structured shape — best effort: serialise it as
                    // a single text part so the model at least sees the JSON.
                    vec![GeminiPart::text(
                        serde_json::to_string(v).unwrap_or_default(),
                    )]
                }
                (_, MessageContent::Multimodal { text, images }) => multimodal_parts(text, images),
            };

            // Merge consecutive same-role turns — Gemini rejects them.
            match contents.last_mut() {
                Some(last) if last.role == role => last.parts.extend(parts),
                _ => contents.push(GeminiContent {
                    role: role.to_string(),
                    parts,
                }),
            }
        }

        (system_instruction, contents)
    }

    /// Map our `ToolDefinition`s onto Gemini's `functionDeclarations` array.
    /// Schemas pass through verbatim — Gemini's published rejections (`$ref`,
    /// `oneOf`, `not`, ...) aren't shapes we currently emit, so we don't
    /// rewrite anything.
    fn build_tools(&self, request: &LlmRequest) -> Option<Vec<GeminiTool>> {
        let defs = request.tools.as_ref()?;
        if defs.is_empty() {
            return None;
        }
        let declarations: Vec<GeminiFunctionDeclaration> = defs
            .iter()
            .map(|td| GeminiFunctionDeclaration {
                name: td.name.clone(),
                description: td.description.clone(),
                parameters: td.parameters.clone(),
            })
            .collect();
        Some(vec![GeminiTool {
            function_declarations: declarations,
        }])
    }

    /// Build the `generationConfig` object. Only includes `thinkingConfig`
    /// when the configured family is a Gemini reasoning family AND the model
    /// slug looks like a 2.5 / 3.x build — Gemini silently ignores it on
    /// other models, but we'd rather not send fields models don't recognise.
    fn build_generation_config(&self, request: &LlmRequest) -> GeminiGenerationConfig {
        let thinking_eligible = matches!(
            self.quirks.reasoning_surface,
            ReasoningSurface::NativeContentBlock
        ) && (self.default_model.contains("2.5")
            || self.default_model.contains("-3"));
        let thinking_config = if thinking_eligible {
            map_gemini_thinking_config(&self.default_model, request.reasoning_effort)
        } else {
            None
        };
        GeminiGenerationConfig {
            temperature: request.temperature,
            max_output_tokens: request.max_tokens,
            thinking_config,
        }
    }

    fn build_request_body(&self, request: &LlmRequest) -> GeminiRequest {
        let (system_instruction, contents) = self.build_contents(request);
        GeminiRequest {
            contents,
            system_instruction,
            tools: self.build_tools(request),
            tool_config: None,
            generation_config: Some(self.build_generation_config(request)),
        }
    }

    /// Map HTTP errors to `AthenError`, lifting Gemini's `error.message` field
    /// out of the body when present so the user sees a useful one-liner.
    fn map_error(&self, status: reqwest::StatusCode, body: &str) -> AthenError {
        let detail = serde_json::from_str::<GeminiErrorBody>(body)
            .ok()
            .map(|e| e.error.message)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| body.to_string());
        let message = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            format!("rate limited: {}", detail)
        } else if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            format!("authentication failed: {}", detail)
        } else if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            format!("server overloaded ({}): {}", status, detail)
        } else {
            format!("HTTP {}: {}", status, detail)
        };
        AthenError::LlmProvider {
            provider: "google".into(),
            message,
        }
    }

    /// Lift a parsed Gemini response into our generic `LlmResponse`. Returns
    /// an error for safety blocks (empty `candidates`) so callers see the
    /// block reason rather than a silent empty turn.
    fn parse_response_value(&self, api_response: GeminiResponse) -> Result<LlmResponse> {
        let candidate = match api_response.candidates.into_iter().next() {
            Some(c) => c,
            None => {
                let reason = api_response
                    .prompt_feedback
                    .as_ref()
                    .and_then(|f| f.block_reason.as_deref())
                    .unwrap_or("unknown");
                return Err(AthenError::LlmProvider {
                    provider: "google".into(),
                    message: format!("content blocked: {}", reason),
                });
            }
        };

        let mut content_chunks: Vec<String> = Vec::new();
        let mut reasoning_chunks: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(content) = candidate.content {
            for part in content.parts {
                if let Some(text) = part.text {
                    if part.thought.unwrap_or(false) {
                        reasoning_chunks.push(text);
                    } else {
                        content_chunks.push(text);
                    }
                }
                if let Some(call) = part.function_call {
                    tool_calls.push(ToolCall {
                        id: call
                            .id
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                        name: call.name,
                        arguments: call.args.unwrap_or(serde_json::Value::Null),
                        // Carry Gemini's thoughtSignature through the
                        // round-trip so the next request can replay it.
                        thought_signature: part.thought_signature,
                    });
                }
            }
        }

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            map_finish_reason(candidate.finish_reason.as_deref())
        };

        let usage = match api_response.usage_metadata {
            Some(u) => TokenUsage {
                prompt_tokens: u.prompt_token_count.unwrap_or(0),
                completion_tokens: u.candidates_token_count.unwrap_or(0),
                total_tokens: u.total_token_count.unwrap_or_else(|| {
                    u.prompt_token_count.unwrap_or(0) + u.candidates_token_count.unwrap_or(0)
                }),
                estimated_cost_usd: Some(estimate_google_cost(
                    api_response
                        .model_version
                        .as_deref()
                        .unwrap_or(self.default_model.as_str()),
                    u.prompt_token_count.unwrap_or(0),
                    u.candidates_token_count.unwrap_or(0),
                )),
                ..TokenUsage::default()
            },
            None => TokenUsage::default(),
        };

        let mut response = LlmResponse {
            content: content_chunks.join(""),
            reasoning_content: if reasoning_chunks.is_empty() {
                None
            } else {
                Some(reasoning_chunks.join(""))
            },
            model_used: api_response
                .model_version
                .unwrap_or_else(|| self.default_model.clone()),
            provider: "google".into(),
            usage,
            tool_calls,
            finish_reason,
        };
        quirks::apply_to_response(&self.quirks, &mut response);
        Ok(response)
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    fn provider_id(&self) -> &str {
        "google"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request_body(request);
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.default_model
        );

        debug!(model = %self.default_model, "sending Google completion request");

        let http_response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AthenError::Timeout(Duration::from_secs(120))
                } else {
                    AthenError::LlmProvider {
                        provider: "google".into(),
                        message: format!("request failed: {}", e),
                    }
                }
            })?;

        let status = http_response.status();
        if !status.is_success() {
            let error_body = http_response.text().await.unwrap_or_default();
            return Err(self.map_error(status, &error_body));
        }

        let api_response: GeminiResponse =
            http_response
                .json()
                .await
                .map_err(|e| AthenError::LlmProvider {
                    provider: "google".into(),
                    message: format!("failed to parse response: {}", e),
                })?;

        self.parse_response_value(api_response)
    }

    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let body = self.build_request_body(request);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.default_model
        );

        debug!(model = %self.default_model, "sending Google streaming request");

        let http_response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AthenError::Timeout(Duration::from_secs(120))
                } else {
                    AthenError::LlmProvider {
                        provider: "google".into(),
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
        let chunk_stream = byte_stream
            .scan(Vec::<u8>::new(), |buffer, result| {
                let emitted: Vec<Result<LlmChunk>> = match result {
                    Ok(bytes) => {
                        buffer.extend_from_slice(&bytes);
                        drain_complete_events(buffer)
                    }
                    Err(e) => vec![Err(AthenError::LlmProvider {
                        provider: "google".into(),
                        message: format!("stream error: {}", e),
                    })],
                };
                futures::future::ready(Some(emitted))
            })
            .flat_map(futures::stream::iter);

        // Fallback cost fill: if the streamed event omitted `modelVersion`
        // the parser couldn't price the usage, so estimate it here from the
        // configured model — mirrors the non-streaming `complete()` path.
        let model = self.default_model.clone();
        let chunk_stream = chunk_stream.map(move |item| match item {
            Ok(mut chunk) => {
                if let Some(usage) = chunk.usage.as_mut() {
                    if usage.estimated_cost_usd.is_none() {
                        usage.estimated_cost_usd = Some(estimate_google_cost(
                            &model,
                            usage.prompt_tokens,
                            usage.completion_tokens,
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
        !self.api_key.is_empty()
    }

    fn supports_vision(&self) -> bool {
        self.supports_vision
    }

    fn supports_documents(&self) -> bool {
        self.supports_documents
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the Gemini `parts` array for a multimodal user turn — a text part
/// (if non-empty) followed by one `inlineData` part per base64 image. URL-
/// source images can't be expressed through `inlineData`, so we warn and
/// skip them.
fn multimodal_parts(text: &str, images: &[ImageInput]) -> Vec<GeminiPart> {
    let mut parts: Vec<GeminiPart> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(GeminiPart::text(text.to_string()));
    }
    for img in images {
        match &img.data {
            ImageData::Base64 { data } => {
                parts.push(GeminiPart::inline_data(img.mime_type.clone(), data.clone()));
            }
            ImageData::Url { url } => {
                warn!(
                    url = url,
                    "Google adapter: URL-source images are not yet supported; skipping (use base64 or upload via the File API)."
                );
            }
        }
    }
    parts
}

/// Turn whatever JSON value the executor handed us for a Tool-role turn into
/// a `functionResponse` part. Accepts either a pre-shaped `{name, response,
/// id?}` blob OR an arbitrary value (wrapped under `response.result`).
fn gemini_function_response_from_value(v: &serde_json::Value) -> GeminiPart {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "tool".to_string());
    let id = v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
    let response = v
        .get("response")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "result": v.clone() }));
    GeminiPart::function_response(name, id, response)
}

fn map_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("STOP") => FinishReason::Stop,
        Some("MAX_TOKENS") => FinishReason::MaxTokens,
        // SAFETY / RECITATION / OTHER all fold into Stop — we don't have
        // a dedicated content-blocked finish reason in the core enum.
        _ => FinishReason::Stop,
    }
}

/// Drain every complete SSE event from `buffer` (events are separated by a
/// blank line — the two-byte sequence `\n\n`). Leaves any trailing partial
/// event in the buffer for the next byte chunk to complete.
///
/// Gemini's JSON payloads never contain raw LF bytes (newlines inside string
/// values arrive as the two characters `\` `n`), so searching the raw buffer
/// for `\n\n` is unambiguous. UTF-8 decoding is deferred until we have a full
/// event so we don't corrupt multi-byte characters split across TCP segments.
fn drain_complete_events(buffer: &mut Vec<u8>) -> Vec<Result<LlmChunk>> {
    let mut out = Vec::new();
    while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
        let event_bytes: Vec<u8> = buffer.drain(..end).collect();
        // Drop the `\n\n` terminator itself.
        buffer.drain(..2);
        let event_text = String::from_utf8_lossy(&event_bytes);
        out.extend(parse_sse_event(&event_text));
    }
    out
}

/// Parse one complete SSE event (the text between two `\n\n` separators) into
/// `LlmChunk` results. An event may contain multiple `data:` lines, which by
/// SSE convention are concatenated; Gemini's stream uses one per event.
fn parse_sse_event(event_text: &str) -> Vec<Result<LlmChunk>> {
    let mut data = String::new();
    for line in event_text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // Per SSE: a single leading space after the colon is part of the
            // protocol, not the payload.
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        return Vec::new();
    }

    let mut event: GeminiResponse = match serde_json::from_str(&data) {
        Ok(e) => e,
        Err(err) => {
            // A complete `data:` line that still fails JSON parse means
            // genuinely malformed payload, not a chunk-split (that case is
            // prevented by the caller buffering until `\n\n`). Surface it
            // loudly — silently dropping tool calls is what got us here.
            warn!(
                error = %err,
                data_len = data.len(),
                "google: failed to parse SSE event data"
            );
            return Vec::new();
        }
    };

    let mut chunks = Vec::new();

    // Capture usage + model before `event.candidates` is consumed below.
    // Gemini reports `usageMetadata` on the final streamed chunk (the one
    // carrying `finishReason`); `cachedContentTokenCount` maps to the
    // discounted cached-prefix portion. Cost is computed here when the model
    // version is present, else left `None` for the streaming method to fill.
    let usage_metadata = event.usage_metadata.take();
    let model_version = event.model_version.clone();

    // Safety block surfaced mid-stream — emit a single error chunk so the
    // consumer sees the reason rather than a silent end-of-stream.
    if event.candidates.is_empty() {
        let reason = event
            .prompt_feedback
            .as_ref()
            .and_then(|f| f.block_reason.as_deref())
            .unwrap_or("unknown");
        chunks.push(Err(AthenError::LlmProvider {
            provider: "google".into(),
            message: format!("content blocked: {}", reason),
        }));
        return chunks;
    }

    let Some(candidate) = event.candidates.into_iter().next() else {
        return chunks;
    };
    let has_finish = candidate
        .finish_reason
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if let Some(content) = candidate.content {
        for part in content.parts {
            if let Some(text) = part.text {
                if !text.is_empty() {
                    chunks.push(Ok(LlmChunk {
                        delta: text,
                        is_final: false,
                        is_thinking: part.thought.unwrap_or(false),
                        tool_calls: vec![],
                        usage: None,
                    }));
                }
            }
            if let Some(call) = part.function_call {
                chunks.push(Ok(LlmChunk {
                    delta: String::new(),
                    is_final: false,
                    is_thinking: false,
                    tool_calls: vec![ToolCall {
                        id: call
                            .id
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                        name: call.name,
                        arguments: call.args.unwrap_or(serde_json::Value::Null),
                        thought_signature: part.thought_signature,
                    }],
                    usage: None,
                }));
            }
        }
    }

    if has_finish {
        let usage = usage_metadata.map(|u| {
            let prompt = u.prompt_token_count.unwrap_or(0);
            let completion = u.candidates_token_count.unwrap_or(0);
            let total = u.total_token_count.unwrap_or(prompt + completion);
            // Cost only when we know the model; otherwise the streaming
            // method fills it from `self.default_model`.
            let estimated_cost_usd = model_version
                .as_deref()
                .map(|m| estimate_google_cost(m, prompt, completion));
            TokenUsage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                estimated_cost_usd,
                cached_tokens: u.cached_content_token_count.filter(|&c| c > 0),
                cache_creation_tokens: None,
            }
        });
        chunks.push(Ok(LlmChunk {
            delta: String::new(),
            is_final: true,
            is_thinking: false,
            tool_calls: vec![],
            usage,
        }));
    }

    chunks
}

/// Parse a buffer that already contains one or more complete SSE events
/// (each terminated by `\n\n`). Kept around as a test-friendly wrapper —
/// production streaming uses [`drain_complete_events`] directly.
#[cfg(test)]
fn parse_sse_chunks(text: &str) -> Vec<Result<LlmChunk>> {
    let mut chunks = Vec::new();
    for event in text.split("\n\n") {
        if event.is_empty() {
            continue;
        }
        chunks.extend(parse_sse_event(event));
    }
    chunks
}

/// Rough per-1M-token pricing for Gemini models (as of 2026). Kept in step
/// with the published pay-as-you-go list pricing — caller is responsible for
/// updating these alongside model launches.
fn estimate_google_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (input_per_m, output_per_m) =
        if model.contains("2.5-pro") || model.contains("3-pro") || model.contains("3.1-pro") {
            (1.25, 10.0)
        } else if model.contains("2.5-flash-lite") {
            (0.10, 0.40)
        } else if model.contains("2.5-flash") || model.contains("flash") {
            (0.30, 2.50)
        } else if model.contains("embedding") {
            (0.15, 0.0)
        } else {
            // Default to flash pricing — closer to the free-tier-friendly model
            // we ship as the default than to Pro.
            (0.30, 2.50)
        };
    let input_cost = (input_tokens as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_per_m;
    input_cost + output_cost
}

// ---------------------------------------------------------------------------
// Gemini API types (outbound + inbound; mostly symmetric)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    #[serde(default)]
    role: String,
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

/// A single Gemini content part. The wire format is an untagged union (any of
/// `text` / `inlineData` / `functionCall` / `functionResponse` may be set on
/// a given part), so we model it as a struct of `Option`s with
/// `skip_serializing_if` — exactly what the Gemini docs show.
#[derive(Debug, Serialize, Deserialize, Default)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Set on response parts when the model emitted this part as part of its
    /// chain of thought. We never send `thought` on outbound parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
    /// Opaque, base64-encoded signature returned by Gemini 3 / thinking-mode
    /// 2.5 on parts that participated in chain-of-thought reasoning — most
    /// importantly on `functionCall` parts. The API REQUIRES that we echo
    /// this back unchanged when replaying the same part in subsequent turns
    /// (HTTP 400 "Function call is missing a thought_signature" otherwise).
    /// We never set this on outbound parts ourselves; it only rides back
    /// when we serialise the model's prior tool call from conversation
    /// history.
    #[serde(
        rename = "thoughtSignature",
        skip_serializing_if = "Option::is_none",
        default
    )]
    thought_signature: Option<String>,
    #[serde(
        rename = "inlineData",
        skip_serializing_if = "Option::is_none",
        default
    )]
    inline_data: Option<GeminiInlineData>,
    #[serde(
        rename = "functionCall",
        skip_serializing_if = "Option::is_none",
        default
    )]
    function_call: Option<GeminiFunctionCall>,
    #[serde(
        rename = "functionResponse",
        skip_serializing_if = "Option::is_none",
        default
    )]
    function_response: Option<GeminiFunctionResponse>,
}

impl GeminiPart {
    fn text(s: String) -> Self {
        Self {
            text: Some(s),
            ..Self::default()
        }
    }

    fn inline_data(mime_type: String, data: String) -> Self {
        Self {
            inline_data: Some(GeminiInlineData { mime_type, data }),
            ..Self::default()
        }
    }

    fn function_call(
        name: String,
        id: Option<String>,
        args: serde_json::Value,
        thought_signature: Option<String>,
    ) -> Self {
        Self {
            thought_signature,
            function_call: Some(GeminiFunctionCall {
                name,
                id,
                args: Some(args),
            }),
            ..Self::default()
        }
    }

    fn function_response(name: String, id: Option<String>, response: serde_json::Value) -> Self {
        Self {
            function_response: Some(GeminiFunctionResponse {
                name,
                id,
                response: Some(response),
            }),
            ..Self::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    args: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    response: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct GeminiToolConfig {
    #[serde(rename = "functionCallingConfig")]
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionCallingConfig {
    mode: String,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Debug, Serialize)]
struct GeminiThinkingConfig {
    /// Gemini 2.5 surface: numeric token budget. `-1` means dynamic
    /// (the previous always-on default before the cross-provider knob
    /// landed). Mutually exclusive with `thinking_level` on the wire —
    /// only one is set per request.
    #[serde(rename = "thinkingBudget", skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i32>,
    /// Gemini 3.x surface: enum (`minimal`, `low`, `medium`, `high`).
    /// Replaces `thinkingBudget` on 3.x models — Google deprecates
    /// budgets there in favour of named levels.
    #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
    #[serde(rename = "includeThoughts", skip_serializing_if = "Option::is_none")]
    include_thoughts: Option<bool>,
}

/// Map our cross-provider `ReasoningEffort` to Gemini's `thinkingConfig`.
/// Gemini 3.x prefers the `thinkingLevel` enum; Gemini 2.5 keeps the
/// older `thinkingBudget` (token count). `Default` returns `None` so
/// neither field is emitted — provider picks its own default.
///
/// `model_slug` is consulted as a coarse 3.x / 2.5 detector. The earlier
/// `default_model.contains("-3")` / `"2.5"` test already gated entry to
/// this helper; here we just pick between the two wire shapes.
fn map_gemini_thinking_config(
    model_slug: &str,
    effort: ReasoningEffort,
) -> Option<GeminiThinkingConfig> {
    if matches!(effort, ReasoningEffort::Default) {
        return None;
    }
    let is_3x = model_slug.contains("-3");
    if is_3x {
        let level = match effort {
            ReasoningEffort::Off => "minimal",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            // Gemini 3 caps at `high`; we collapse Max → high rather
            // than 400ing on a value the model doesn't accept.
            ReasoningEffort::High | ReasoningEffort::Max => "high",
            ReasoningEffort::Default => unreachable!(),
        };
        Some(GeminiThinkingConfig {
            thinking_budget: None,
            thinking_level: Some(level),
            include_thoughts: Some(true),
        })
    } else {
        // Gemini 2.5 family: token budgets per the design doc table.
        let budget: i32 = match effort {
            ReasoningEffort::Off => 0,
            ReasoningEffort::Minimal => 1024,
            ReasoningEffort::Low => 4096,
            ReasoningEffort::Medium => 12_288,
            ReasoningEffort::High | ReasoningEffort::Max => 24_576,
            ReasoningEffort::Default => unreachable!(),
        };
        Some(GeminiThinkingConfig {
            thinking_budget: Some(budget),
            thinking_level: None,
            include_thoughts: Some(true),
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "promptFeedback", default)]
    prompt_feedback: Option<GeminiPromptFeedback>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(rename = "modelVersion", default)]
    model_version: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiPromptFeedback {
    #[serde(rename = "blockReason", default)]
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: Option<u32>,
    #[serde(rename = "totalTokenCount", default)]
    total_token_count: Option<u32>,
    /// Portion of `promptTokenCount` served from Gemini's context cache.
    /// Surfaced into `TokenUsage.cached_tokens`. Absent on responses without
    /// caching, hence `default`.
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GeminiErrorBody {
    error: GeminiError,
}

#[derive(Debug, Deserialize)]
struct GeminiError {
    #[serde(default)]
    message: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::llm::ModelFamily;

    fn provider_with_family(family: ModelFamily) -> GoogleProvider {
        GoogleProvider::new("test-key".into(), "gemini-2.5-flash".into()).with_family(family)
    }

    fn user_text(s: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: MessageContent::Text(s.into()),
        }
    }

    #[test]
    fn multimodal_emits_text_then_inline_data() {
        let images = vec![ImageInput {
            mime_type: "image/png".into(),
            data: ImageData::Base64 {
                data: "AAAA".into(),
            },
        }];
        let parts = multimodal_parts("describe this", &images);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_deref(), Some("describe this"));
        let inline = parts[1].inline_data.as_ref().expect("inline data");
        assert_eq!(inline.mime_type, "image/png");
        assert_eq!(inline.data, "AAAA");
    }

    #[test]
    fn multimodal_url_image_is_skipped_with_warning() {
        let images = vec![ImageInput {
            mime_type: "image/jpeg".into(),
            data: ImageData::Url {
                url: "https://example.com/x.jpg".into(),
            },
        }];
        let parts = multimodal_parts("look", &images);
        // Only the text part survives — URL images are not yet supported.
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_deref(), Some("look"));
    }

    #[test]
    fn pdf_rides_through_inline_data() {
        let images = vec![ImageInput {
            mime_type: "application/pdf".into(),
            data: ImageData::Base64 {
                data: "JVBE".into(),
            },
        }];
        let parts = multimodal_parts("summarise", &images);
        let inline = parts[1].inline_data.as_ref().unwrap();
        assert_eq!(inline.mime_type, "application/pdf");
    }

    #[test]
    fn tool_role_message_serializes_as_function_response() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                user_text("call the search tool"),
                ChatMessage {
                    role: Role::Tool,
                    content: MessageContent::Structured(serde_json::json!({
                        "name": "search",
                        "id": "call_abc",
                        "response": { "result": "found it" }
                    })),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let (_sys, contents) = provider.build_contents(&req);
        // Both messages map to `user` and merge into one turn with two parts.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts.len(), 2);
        let fr = contents[0].parts[1]
            .function_response
            .as_ref()
            .expect("function response");
        assert_eq!(fr.name, "search");
        assert_eq!(fr.id.as_deref(), Some("call_abc"));
        assert_eq!(
            fr.response.as_ref().unwrap()["result"],
            serde_json::Value::String("found it".into())
        );
    }

    #[test]
    fn tool_role_text_fallback_wraps_in_response_result() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                user_text("hi"),
                ChatMessage {
                    role: Role::Tool,
                    content: MessageContent::Text("plain string result".into()),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let (_sys, contents) = provider.build_contents(&req);
        let fr = contents[0].parts[1]
            .function_response
            .as_ref()
            .expect("function response");
        assert_eq!(fr.name, "tool");
        assert_eq!(
            fr.response.as_ref().unwrap()["result"],
            serde_json::Value::String("plain string result".into())
        );
    }

    #[test]
    fn system_message_lifted_to_system_instruction() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: MessageContent::Text("you are a helpful agent".into()),
                },
                user_text("hi"),
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let (sys, contents) = provider.build_contents(&req);
        let sys = sys.expect("system instruction populated");
        assert_eq!(sys.parts.len(), 1);
        assert_eq!(
            sys.parts[0].text.as_deref(),
            Some("you are a helpful agent")
        );
        // Only the user message remains in `contents`.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn missing_system_means_field_is_omitted() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let (sys, _contents) = provider.build_contents(&req);
        assert!(sys.is_none());
        // And the request body must not include the field.
        let body = provider.build_request_body(&req);
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("systemInstruction").is_none());
    }

    #[test]
    fn consecutive_user_turns_merge_into_one_content() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                user_text("first half"),
                user_text("second half"),
                ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let (_sys, contents) = provider.build_contents(&req);
        // Two consecutive user messages → one merged user turn, then one model turn.
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts.len(), 2);
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn thinking_config_omitted_for_default_family() {
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let cfg = provider.build_generation_config(&req);
        assert!(cfg.thinking_config.is_none());
    }

    #[test]
    fn thinking_config_omitted_when_effort_is_default() {
        // Per the cross-provider design: `Default` means "send nothing,
        // let the provider apply its built-in default". This replaces
        // the old always-on `thinking_budget: -1` behaviour — the user
        // now opts in via the per-arc reasoning_effort override.
        let provider = GoogleProvider::new("k".into(), "gemini-3.1-pro".into())
            .with_family(ModelFamily::Gemini3Pro);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let cfg = provider.build_generation_config(&req);
        assert!(cfg.thinking_config.is_none(), "Default must omit");
    }

    #[test]
    fn gemini3_uses_thinking_level_enum() {
        let provider = GoogleProvider::new("k".into(), "gemini-3.1-pro".into())
            .with_family(ModelFamily::Gemini3Pro);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::High,
        };
        let cfg = provider.build_generation_config(&req);
        let tc = cfg.thinking_config.expect("present");
        assert_eq!(tc.thinking_level, Some("high"));
        assert_eq!(tc.thinking_budget, None);
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn gemini3_max_clamps_to_high_on_wire() {
        // Gemini 3 enum has no `xhigh`; we collapse Max → high.
        let provider = GoogleProvider::new("k".into(), "gemini-3.1-pro".into())
            .with_family(ModelFamily::Gemini3Pro);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::Max,
        };
        let cfg = provider.build_generation_config(&req);
        assert_eq!(cfg.thinking_config.unwrap().thinking_level, Some("high"));
    }

    #[test]
    fn parse_response_extracts_thought_and_text_separately() {
        let provider = provider_with_family(ModelFamily::Gemini3Pro);
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "text": "Let me think...", "thought": true },
                        { "text": "The answer is 42." }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            },
            "modelVersion": "gemini-3.1-pro"
        });
        let api: GeminiResponse = serde_json::from_value(raw).unwrap();
        let resp = provider.parse_response_value(api).unwrap();
        assert_eq!(resp.content, "The answer is 42.");
        assert_eq!(resp.reasoning_content.as_deref(), Some("Let me think..."));
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 5);
    }

    #[test]
    fn parse_response_extracts_function_call() {
        let provider = provider_with_family(ModelFamily::Default);
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "functionCall": {
                            "name": "search",
                            "args": { "q": "foo" }
                        }}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let api: GeminiResponse = serde_json::from_value(raw).unwrap();
        let resp = provider.parse_response_value(api).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[0].arguments["q"], "foo");
        // Tool call presence overrides STOP → ToolUse.
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        // ID was missing on the wire; we generated one.
        assert!(!resp.tool_calls[0].id.is_empty());
    }

    #[test]
    fn parse_response_safety_block_returns_error() {
        let provider = provider_with_family(ModelFamily::Default);
        let raw = serde_json::json!({
            "candidates": [],
            "promptFeedback": { "blockReason": "SAFETY" }
        });
        let api: GeminiResponse = serde_json::from_value(raw).unwrap();
        let err = provider.parse_response_value(api).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("content blocked"));
        assert!(msg.contains("SAFETY"));
    }

    #[test]
    fn cost_estimation_matches_per_model_pricing() {
        // Flash: $0.30 / $2.50 per M
        let flash = estimate_google_cost("gemini-2.5-flash", 1_000_000, 1_000_000);
        assert!((flash - 2.80).abs() < 1e-6, "flash cost = {flash}");

        // Pro: $1.25 / $10 per M
        let pro = estimate_google_cost("gemini-2.5-pro", 1_000_000, 1_000_000);
        assert!((pro - 11.25).abs() < 1e-6, "pro cost = {pro}");

        // Flash Lite: $0.10 / $0.40 per M
        let lite = estimate_google_cost("gemini-2.5-flash-lite", 1_000_000, 1_000_000);
        assert!((lite - 0.50).abs() < 1e-6, "lite cost = {lite}");

        // Embedding: input only.
        let emb = estimate_google_cost("text-embedding-004", 1_000_000, 1_000_000);
        assert!((emb - 0.15).abs() < 1e-6, "emb cost = {emb}");
    }

    #[test]
    fn sse_chunk_with_text_part_yields_text_chunk() {
        let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello\"}]}}]}\n\n";
        let chunks = parse_sse_chunks(sse);
        let oks: Vec<_> = chunks.into_iter().filter_map(|c| c.ok()).collect();
        assert_eq!(oks.len(), 1);
        assert_eq!(oks[0].delta, "hello");
        assert!(!oks[0].is_final);
        assert!(!oks[0].is_thinking);
    }

    #[test]
    fn sse_chunk_with_thought_part_marks_chunk_as_thinking() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hmm\",\"thought\":true}]}}]}\n\n";
        let oks: Vec<_> = parse_sse_chunks(sse)
            .into_iter()
            .filter_map(|c| c.ok())
            .collect();
        assert_eq!(oks.len(), 1);
        assert!(oks[0].is_thinking);
        assert_eq!(oks[0].delta, "hmm");
    }

    #[test]
    fn sse_final_chunk_emitted_on_finish_reason() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let oks: Vec<_> = parse_sse_chunks(sse)
            .into_iter()
            .filter_map(|c| c.ok())
            .collect();
        // One text delta + one final marker.
        assert_eq!(oks.len(), 2);
        assert_eq!(oks[0].delta, "done");
        assert!(oks[1].is_final);
    }

    #[test]
    fn build_tools_passes_schema_through_verbatim() {
        use athen_core::tool::ToolDefinition;
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![user_text("hi")],
            max_tokens: None,
            temperature: None,
            tools: Some(vec![ToolDefinition {
                name: "search".into(),
                description: "search the web".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "q": { "type": "string" } },
                    "required": ["q"]
                }),
                backend: athen_core::tool::ToolBackend::Shell {
                    command: "true".into(),
                    native: true,
                },
                base_risk: athen_core::risk::BaseImpact::Read,
            }]),
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let tools = provider.build_tools(&req).expect("tools populated");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function_declarations.len(), 1);
        let decl = &tools[0].function_declarations[0];
        assert_eq!(decl.name, "search");
        assert_eq!(decl.parameters["required"][0], "q");
    }

    #[test]
    fn drain_complete_events_recovers_function_call_split_across_two_chunks() {
        // Regression: a `write` functionCall part with a large args payload
        // overflows a single TCP segment. Old per-chunk parser warn-dropped
        // the half it couldn't parse, losing both the tool call and its
        // thoughtSignature (the latter then crashed the next turn with
        // HTTP 400 "missing thought_signature").
        let event_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "write",
                            "args": {
                                "path": "/tmp/x.html",
                                "content": "<!DOCTYPE html>\n<html>\n<body>\n<p>hello</p>\n</body>\n</html>"
                            }
                        },
                        "thoughtSignature": "SIG-CHUNK-SPLIT"
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let event = format!("data: {}\n\n", serde_json::to_string(&event_json).unwrap());
        let bytes = event.as_bytes();
        // Slice at an arbitrary mid-payload offset (inside the JSON string body).
        let split = bytes.len() / 2;

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&bytes[..split]);
        let first_pass = drain_complete_events(&mut buf);
        // The terminator `\n\n` hasn't arrived yet — nothing should be drained.
        assert!(
            first_pass.is_empty(),
            "expected zero chunks before terminator, got {first_pass:?}"
        );
        // Buffer still holds the partial event for the next chunk.
        assert!(!buf.is_empty());

        buf.extend_from_slice(&bytes[split..]);
        let second_pass = drain_complete_events(&mut buf);
        let oks: Vec<_> = second_pass.into_iter().filter_map(|c| c.ok()).collect();
        // Expect: one tool_call chunk + one final marker.
        assert_eq!(oks.len(), 2, "unexpected chunk count: {oks:?}");
        assert_eq!(oks[0].tool_calls.len(), 1);
        assert_eq!(oks[0].tool_calls[0].name, "write");
        assert_eq!(
            oks[0].tool_calls[0].thought_signature.as_deref(),
            Some("SIG-CHUNK-SPLIT"),
            "thoughtSignature must survive cross-chunk reassembly"
        );
        assert!(oks[1].is_final);
        // Buffer fully drained.
        assert!(buf.is_empty(), "expected empty buffer, got {buf:?}");
    }

    #[test]
    fn drain_complete_events_handles_two_events_in_one_chunk() {
        let event_a = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n";
        let event_b = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"there\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(event_a.as_bytes());
        buf.extend_from_slice(event_b.as_bytes());
        let oks: Vec<_> = drain_complete_events(&mut buf)
            .into_iter()
            .filter_map(|c| c.ok())
            .collect();
        // hi + there + final marker.
        assert_eq!(oks.len(), 3);
        assert_eq!(oks[0].delta, "hi");
        assert_eq!(oks[1].delta, "there");
        assert!(oks[2].is_final);
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_response_captures_thought_signature_on_function_call() {
        // Gemini 3 / thinking-mode responses tag the functionCall part with
        // an opaque thoughtSignature blob. We must lift it onto the ToolCall
        // or the very next request will be rejected as "missing thought_signature".
        let provider = provider_with_family(ModelFamily::Gemini3Pro);
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": { "name": "web_search", "args": { "q": "x" } },
                        "thoughtSignature": "AAAA-opaque-blob-zzzz"
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let api: GeminiResponse = serde_json::from_value(raw).unwrap();
        let resp = provider.parse_response_value(api).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(
            resp.tool_calls[0].thought_signature.as_deref(),
            Some("AAAA-opaque-blob-zzzz")
        );
    }

    #[test]
    fn assistant_history_round_trips_thought_signature() {
        // When the executor replays a prior assistant tool-call turn, the
        // builder must serialise the signature back onto the functionCall
        // part — otherwise Gemini 3 rejects the turn.
        let provider = provider_with_family(ModelFamily::Gemini3Pro);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                user_text("hi"),
                ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Structured(serde_json::json!({
                        "text": "",
                        "tool_calls": [{
                            "id": "call_abc",
                            "name": "web_search",
                            "arguments": { "q": "x" },
                            "thought_signature": "SIGN-XYZ"
                        }]
                    })),
                },
                ChatMessage {
                    role: Role::Tool,
                    content: MessageContent::Structured(serde_json::json!({
                        "name": "web_search",
                        "id": "call_abc",
                        "response": { "result": "ok" }
                    })),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let body = provider.build_request_body(&req);
        let json = serde_json::to_value(&body).unwrap();
        // Find the model turn's functionCall part and check it carries the
        // signature at the part level (Gemini wire format).
        let model_turn = json["contents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["role"] == "model")
            .expect("model turn present");
        let part = &model_turn["parts"][0];
        assert_eq!(part["functionCall"]["name"], "web_search");
        assert_eq!(part["thoughtSignature"], "SIGN-XYZ");
    }

    #[test]
    fn assistant_history_without_signature_does_not_emit_field() {
        // Outbound parts should never carry a signature when the upstream
        // ToolCall didn't supply one — Gemini 2.5 non-thinking responses
        // omit it, and round-tripping `null` would be wrong.
        let provider = provider_with_family(ModelFamily::Default);
        let req = LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![
                user_text("hi"),
                ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Structured(serde_json::json!({
                        "text": "",
                        "tool_calls": [{
                            "id": "call_abc",
                            "name": "web_search",
                            "arguments": { "q": "x" }
                        }]
                    })),
                },
            ],
            max_tokens: None,
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: ReasoningEffort::default(),
        };
        let body = provider.build_request_body(&req);
        let json = serde_json::to_value(&body).unwrap();
        let model_turn = json["contents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["role"] == "model")
            .expect("model turn present");
        let part = &model_turn["parts"][0];
        assert!(part.get("thoughtSignature").is_none());
    }

    #[test]
    fn map_finish_reason_handles_known_and_unknown() {
        assert_eq!(map_finish_reason(Some("STOP")), FinishReason::Stop);
        assert_eq!(
            map_finish_reason(Some("MAX_TOKENS")),
            FinishReason::MaxTokens
        );
        assert_eq!(map_finish_reason(Some("SAFETY")), FinishReason::Stop);
        assert_eq!(map_finish_reason(None), FinishReason::Stop);
    }
}
