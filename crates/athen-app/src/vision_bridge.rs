//! Describe-and-inject vision bridge (Phase 4 of vision support).
//!
//! When a turn carries one or more images, the model that will actually
//! handle the run may or may not be able to *see*. This bridge runs once,
//! in `athen-app`, right before the executor is built — keyed on the arc's
//! **resolved primary model's** vision capability
//! ([`crate::state::arc_primary_supports_vision`], Phase 1):
//!
//! - **primary can see** → images are handed to the executor untouched
//!   (`initial_user_images`), so the vision-capable provider inlines them.
//! - **primary can't see, a Bundle vision slot is configured** → each image
//!   is described *once* by the vision-slot model
//!   ([`crate::state::build_vision_provider`], Phase 2), the description is
//!   cached deterministically-once (Phase 3), and the raw bytes are dropped
//!   in favour of `\n[image: <description>]` text appended to the user turn.
//!   The non-vision provider therefore never receives a `Multimodal` message
//!   it would reject (`reject_multimodal`).
//! - **primary can't see, NO vision slot** → the raw bytes are dropped, a
//!   `\n[image: no vision model configured]` placeholder is appended, and a
//!   user-facing notice is surfaced. Never crashes.
//!
//! It lives here (not in `athen-agent`) because the executor is decoupled
//! from Bundles/`AppState`; the arc's primary model is fixed per run, so a
//! single pre-pass before the executor starts is sufficient (the executor
//! replays the same already-transformed conversation each iteration).

use athen_core::llm::{
    ChatMessage, ImageInput, LlmRequest, MessageContent, ModelProfile, ReasoningEffort, Role,
};
use athen_core::traits::llm::LlmProvider;
use athen_persistence::arcs::ArcStore;

use crate::ui_bridge::UiBridge;

/// Outcome of running the bridge over one turn's images.
pub(crate) struct VisionBridgeResult {
    /// Images to hand the executor as `initial_user_images`. Empty when the
    /// primary model can't see (the raw bytes were replaced by text).
    pub images: Vec<ImageInput>,
    /// Text to append to the user turn so a text-only model still "reads"
    /// the image(s). `None` when the primary can see (images sent inline) or
    /// there were no images to begin with.
    pub injected_text: Option<String>,
}

/// The describe-and-inject pre-pass. See the module docs for the three cases.
///
/// `entry_id` is a stable per-turn key (derived from the turn's owning
/// attachment/sense event via [`event_cache_id`]) so a cached description
/// survives arc reload and is reused verbatim (regenerating would break
/// per-model prompt caching). `image_index` is the position of the image
/// within `images` — callers must pass the images in the same order every
/// run (composer images then surfaced attachments) so indices stay stable.
pub(crate) async fn prepare_images_for_vision(
    images: Vec<ImageInput>,
    primary_supports_vision: bool,
    vision_provider: Option<Box<dyn LlmProvider>>,
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    entry_id: i64,
    ui: &UiBridge,
) -> VisionBridgeResult {
    // Primary can see, or nothing to do → send the images as-is.
    if primary_supports_vision || images.is_empty() {
        return VisionBridgeResult {
            images,
            injected_text: None,
        };
    }

    // Non-vision primary + NO vision slot → never crash: drop the raw bytes,
    // inject a placeholder per image, surface a notice.
    let Some(vision_provider) = vision_provider else {
        tracing::warn!(
            arc_id,
            images = images.len(),
            "vision bridge: primary model can't see and no Bundle vision slot is configured; \
             injecting placeholder text instead of image bytes"
        );
        notify(
            ui,
            arc_id,
            "No vision model configured",
            &format!(
                "This turn included {} image(s) but the active model can't see them and no \
                 Vision model is set. Open Settings → Bundles and pick a Vision model so a \
                 text-only model can read images.",
                images.len()
            ),
        );
        let injected: String = images
            .iter()
            .map(|_| "\n[image: no vision model configured]".to_string())
            .collect();
        return VisionBridgeResult {
            images: Vec::new(),
            injected_text: Some(injected),
        };
    };

    // Non-vision primary + vision slot → describe each image once (cache
    // first), inject the description text. The non-vision provider never
    // sees raw bytes.
    let mut injected = String::new();
    for (idx, image) in images.iter().enumerate() {
        let image_index = idx as i64;
        let description =
            resolve_description(&*vision_provider, arc_store, arc_id, entry_id, image_index, image)
                .await
                .unwrap_or_else(|| {
                    notify(
                        ui,
                        arc_id,
                        "Couldn't read an image",
                        "The Vision model failed to describe an attached image. The text-only \
                         model will proceed without it.",
                    );
                    "(vision model failed to describe this image)".to_string()
                });
        injected.push('\n');
        injected.push_str("[image: ");
        injected.push_str(&description);
        injected.push(']');
    }

    VisionBridgeResult {
        images: Vec::new(),
        injected_text: Some(injected),
    }
}

/// Cache-first description resolution: rehydrated field → persisted
/// side-table → generate once + persist. Returns `None` only when
/// generation itself failed.
async fn resolve_description(
    vision_provider: &dyn LlmProvider,
    arc_store: Option<&ArcStore>,
    arc_id: &str,
    entry_id: i64,
    image_index: i64,
    image: &ImageInput,
) -> Option<String> {
    // 1. Already carried on the image (rehydrated from a prior run).
    if let Some(d) = image.description.as_ref().filter(|d| !d.trim().is_empty()) {
        return Some(d.clone());
    }
    // 2. Persisted side-table (Phase 3).
    if let Some(store) = arc_store {
        match store
            .get_image_description(arc_id, entry_id, image_index)
            .await
        {
            Ok(Some(d)) if !d.trim().is_empty() => return Some(d),
            Ok(_) => {}
            Err(e) => tracing::warn!(arc_id, image_index, "read cached image description: {e}"),
        }
    }
    // 3. Generate once via the vision model, then persist (INSERT OR IGNORE =
    //    deterministic-once even under a concurrent writer).
    let described = describe_image(vision_provider, image).await;
    match described {
        Ok(d) if !d.trim().is_empty() => {
            let d = d.trim().to_string();
            if let Some(store) = arc_store {
                if let Err(e) = store
                    .set_image_description_if_absent(arc_id, entry_id, image_index, &d)
                    .await
                {
                    tracing::warn!(arc_id, image_index, "persist image description: {e}");
                }
            }
            Some(d)
        }
        Ok(_) => {
            tracing::warn!(arc_id, image_index, "vision model returned an empty description");
            Some("(vision model returned an empty description)".to_string())
        }
        Err(e) => {
            tracing::warn!(arc_id, image_index, "vision describe failed: {e}");
            None
        }
    }
}

/// One non-streaming completion against the vision-slot model.
async fn describe_image(
    vision_provider: &dyn LlmProvider,
    image: &ImageInput,
) -> athen_core::error::Result<String> {
    vision_completion(
        vision_provider,
        image,
        "Describe this image in full detail for a text-only model that cannot see \
         it. Transcribe any visible text verbatim, and describe layout, objects, \
         people, colours, and any data (tables, numbers, charts) precisely.",
    )
    .await
}

/// Answer a specific question about one image via the vision-slot model. Powers
/// the Phase 5 `see_image` tool — the lossless on-demand deeper look ("read the
/// exact number in cell B3"). One non-streaming Multimodal completion; the raw
/// bytes are sent to the vision provider only (never to the non-vision primary).
pub(crate) async fn answer_about_image(
    vision_provider: &dyn LlmProvider,
    image: &ImageInput,
    question: &str,
) -> athen_core::error::Result<String> {
    let prompt = format!(
        "Look at this image and answer the question precisely, using only what is \
         actually visible. Transcribe any relevant text or numbers verbatim. If the \
         answer isn't determinable from the image, say so.\n\nQuestion: {question}"
    );
    vision_completion(vision_provider, image, &prompt).await
}

/// Shared single-image, non-streaming completion against the vision-slot
/// provider. The vision slot is a standalone provider instance (not the tiered
/// router); `profile` is inert here but must be a valid variant.
async fn vision_completion(
    vision_provider: &dyn LlmProvider,
    image: &ImageInput,
    prompt: &str,
) -> athen_core::error::Result<String> {
    let request = LlmRequest {
        profile: ModelProfile::Powerful,
        messages: vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Multimodal {
                text: prompt.to_string(),
                images: vec![image.clone()],
            },
        }],
        max_tokens: Some(1024),
        temperature: Some(0.2),
        tools: None,
        system_prompt: None,
        reasoning_effort: ReasoningEffort::Default,
    };
    let response = vision_provider.complete(&request).await?;
    Ok(response.content)
}

/// Surface a user-facing notice through the existing in-app notification
/// event (the same `"notification"` event `InAppChannel` emits). Best-effort:
/// dropped silently when no GUI/bus subscriber is attached.
fn notify(ui: &UiBridge, arc_id: &str, title: &str, body: &str) {
    ui.emit(
        "notification",
        serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "urgency": "normal",
            "title": title,
            "body": body,
            "arc_id": arc_id,
            "requires_response": false,
        }),
    );
}

/// Derive a stable, positive `i64` cache key from the turn's owning event
/// id. The same event id resolves on every run/reload for the same turn's
/// images (composer uploads are persisted under `upload_event_id` and
/// re-surfaced from the `AttachmentStore` under that same id), so this keeps
/// `(arc_id, entry_id, image_index)` reload-stable without threading an
/// `arc_entries.id` through the persist path.
pub(crate) fn event_cache_id(event_id: uuid::Uuid) -> i64 {
    let bytes = event_id.as_bytes();
    let hi = u64::from_be_bytes(bytes[0..8].try_into().expect("uuid is 16 bytes"));
    // Shift out the sign bit so the key is always non-negative.
    (hi >> 1) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_cache_id_is_stable_and_non_negative() {
        let id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(event_cache_id(id), 0);
        let id2 = uuid::Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let k = event_cache_id(id2);
        assert!(k >= 0, "cache id must be non-negative, got {k}");
        // Deterministic: same input → same key.
        assert_eq!(event_cache_id(id2), k);
    }
}
