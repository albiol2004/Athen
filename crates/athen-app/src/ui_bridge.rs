//! Seam between the autonomous core and the (optional) Tauri GUI.
//!
//! Everything in the sense → coordinator → dispatch pipeline used to take a
//! raw `tauri::AppHandle` for two things: emitting frontend events and
//! reaching the managed `AppState`. Headless mode has neither a window nor
//! a Tauri runtime, so those call sites take a [`UiBridge`] instead:
//!
//! - `Tauri(handle)` — desktop mode; `emit` forwards to the WebView,
//!   `app_state` resolves through Tauri's managed state.
//! - `Headless` — daemon mode; `emit` drops the event at DEBUG (the GUI is
//!   the only consumer of those events), `app_state` resolves through a
//!   process-global set once by the headless composition root.
//!
//! Components that are *inherently* GUI-bound (InApp notification channel,
//! InApp approval sink, `place_call`'s resource-dir lookup) still take the
//! real `AppHandle`; the bridge exposes it via [`UiBridge::tauri_handle`]
//! and those components are simply not constructed in headless mode.

use std::sync::{Arc, OnceLock};

use crate::state::AppState;

/// Set once by the headless composition root right after the fully
/// initialized `AppState` is wrapped in its final `Arc`. Background loops
/// started a moment earlier resolve state lazily (first sense event /
/// dispatch), so `wait()` in [`UiBridge::app_state`] only ever blocks for
/// the microseconds between loop spawn and publish.
static HEADLESS_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

/// Handle the autonomous core uses to reach the UI layer, if any.
#[derive(Clone)]
pub enum UiBridge {
    /// Desktop mode: a real Tauri app with a WebView frontend.
    Tauri(tauri::AppHandle),
    /// Daemon mode: no GUI; Telegram is the user surface.
    // TODO(headless): constructed by the headless composition root —
    // remove the allow when it lands.
    #[allow(dead_code)]
    Headless,
}

impl UiBridge {
    /// Publish the headless `AppState` singleton. Must be called exactly
    /// once, by the headless composition root only.
    #[allow(dead_code)] // TODO(headless): used by the headless composition root.
    pub fn publish_headless_state(state: Arc<AppState>) {
        if HEADLESS_STATE.set(state).is_err() {
            tracing::error!("publish_headless_state called twice; keeping the first");
        }
    }

    /// Emit a frontend event. In headless mode the GUI is the only
    /// consumer, so the event is dropped (logged at DEBUG).
    pub fn emit<S: serde::Serialize + Clone>(&self, event: &str, payload: S) {
        match self {
            UiBridge::Tauri(h) => {
                use tauri::Emitter;
                let _ = h.emit(event, payload);
            }
            UiBridge::Headless => {
                tracing::debug!(event, "ui event dropped (headless)");
            }
        }
    }

    /// Resolve the live `AppState` for this process.
    pub fn app_state(&self) -> &AppState {
        match self {
            UiBridge::Tauri(h) => {
                use tauri::Manager;
                h.state::<AppState>().inner()
            }
            UiBridge::Headless => HEADLESS_STATE.wait(),
        }
    }

    /// The raw Tauri handle, when running with a GUI. Components that are
    /// inherently GUI-bound (InApp channels, telephony resource lookup)
    /// gate on this and degrade gracefully when `None`.
    pub fn tauri_handle(&self) -> Option<&tauri::AppHandle> {
        match self {
            UiBridge::Tauri(h) => Some(h),
            UiBridge::Headless => None,
        }
    }
}
