//! User-visible notification sink for non-app crates.
//!
//! Crates that live below `athen-app` (notably `athen-agent`) cannot depend
//! on the Tauri composition root, but they sometimes need to surface a
//! user-visible warning — e.g. the sandbox falling back to unsandboxed
//! shell. This trait gives them a narrow, async-safe seam.
//!
//! Implementations live in `athen-app` and typically forward to the same
//! plumbing used by [`crate::notification::Notification`] consumers
//! (in-app Tauri event + the `NotificationOrchestrator` for Telegram-away
//! delivery).

use async_trait::async_trait;

use crate::notification::Notification;

/// Sink for one-shot user-visible notifications emitted by non-app crates.
#[async_trait]
pub trait UserNotifier: Send + Sync {
    /// Fire-and-forget. Implementations must never panic and should
    /// swallow transport errors — callers treat this as best-effort.
    async fn notify(&self, n: Notification);
}
