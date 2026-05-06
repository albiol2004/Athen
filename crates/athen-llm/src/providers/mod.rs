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
    let has_images = request
        .messages
        .iter()
        .any(|m| matches!(&m.content, MessageContent::Multimodal { images, .. } if !images.is_empty()));
    if has_images {
        return Err(AthenError::LlmProvider {
            provider: provider_id.into(),
            message: "this provider/model does not support image input. Configure a vision-capable provider in Settings before sending images.".into(),
        });
    }
    Ok(())
}
