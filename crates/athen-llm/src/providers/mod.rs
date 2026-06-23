//! LLM provider adapters.

pub mod anthropic;
pub mod deepseek;
pub mod google;
pub mod llamacpp;
pub mod ollama;
pub mod openai;

use athen_core::error::{AthenError, Result};
use athen_core::llm::{LlmRequest, MessageContent};

/// Return an error if the request carries multimodal (image) content but the
/// provider can't actually process it. Vision-incapable adapters call this
/// at the top of `complete`/`complete_streaming` so images surface a clear
/// error instead of being silently dropped on the wire.
pub(crate) fn reject_multimodal(provider_id: &str, request: &LlmRequest) -> Result<()> {
    let has_images = request.messages.iter().any(
        |m| matches!(&m.content, MessageContent::Multimodal { images, .. } if !images.is_empty()),
    );
    if has_images {
        return Err(AthenError::LlmProvider {
            provider: provider_id.into(),
            message: "this provider/model does not support image input. Configure a vision-capable provider in Settings before sending images.".into(),
        });
    }
    Ok(())
}

/// Whether an HTTP status code from a provider should be treated as a
/// *transient* failure worth retrying: 429 (rate limit) or any 5xx
/// (server overloaded / bad gateway / gateway timeout). Permanent failures
/// (4xx other than 429 — auth, bad request, unknown model) fail fast.
pub(crate) fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Parse a `Retry-After` header into whole seconds. Supports both the
/// delta-seconds form (`Retry-After: 30`) and the HTTP-date form
/// (`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`). Returns `None` if the
/// header is absent or unparseable. The router caps this before sleeping,
/// so a hostile value can't hang the agent.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();

    // delta-seconds
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs);
    }

    // HTTP-date — compute seconds until that instant (clamped at 0).
    let when = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let now = chrono::Utc::now();
    let delta = when.with_timezone(&chrono::Utc) - now;
    Some(delta.num_seconds().max(0) as u64)
}

/// Map a `reqwest` send error to an [`AthenError`], classifying transient
/// connection-level failures as retryable. Timeouts become
/// [`AthenError::Timeout`]; connection resets / dropped sockets become
/// [`AthenError::LlmTransient`]; everything else stays a permanent
/// [`AthenError::LlmProvider`]. `context` is a short label ("request failed").
pub(crate) fn map_send_error(provider_id: &str, context: &str, e: reqwest::Error) -> AthenError {
    if e.is_timeout() {
        AthenError::Timeout(std::time::Duration::from_secs(120))
    } else if is_transient_reqwest_error(&e) {
        AthenError::LlmTransient {
            provider: provider_id.into(),
            message: format!("{}: {}", context, e),
            retry_after_secs: None,
        }
    } else {
        AthenError::LlmProvider {
            provider: provider_id.into(),
            message: format!("{}: {}", context, e),
        }
    }
}

/// Whether a `reqwest` send error is a transient connection-level failure
/// (connection reset, dropped socket, request/body transmission error)
/// rather than a timeout (handled separately) or a programming error.
pub(crate) fn is_transient_reqwest_error(e: &reqwest::Error) -> bool {
    // Connect failures and broken connections are safe to retry. `is_request`
    // covers errors raised while sending the request/body (resets fall here).
    // We deliberately do not treat builder/redirect/decode errors as transient.
    e.is_connect() || e.is_request()
}
