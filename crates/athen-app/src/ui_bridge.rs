//! Seam between the autonomous core and the (optional) Tauri GUI.
//!
//! Everything in the sense → coordinator → dispatch pipeline used to take a
//! raw `tauri::AppHandle` for two things: emitting frontend events and
//! reaching the managed `AppState`. Headless mode has neither a window nor
//! a Tauri runtime, so those call sites take a [`UiBridge`] instead:
//!
//! - `Tauri(handle)` — desktop mode; `emit` forwards to the WebView,
//!   `app_state` resolves through Tauri's managed state.
//! - `Headless` — daemon mode; `app_state` resolves through a
//!   process-global set once by the headless composition root.
//!
//! In both modes, `emit` additionally fans out to the process-global
//! event bus when the HTTP API is enabled (`http_api::serve` → SSE for
//! remote React / React Native clients); a headless emit with no bus is
//! dropped at DEBUG.
//!
//! Components that are *inherently* GUI-bound (InApp notification channel,
//! `place_call`'s resource-dir lookup) still take the real `AppHandle`;
//! the bridge exposes it via [`UiBridge::tauri_handle`] and those
//! components are simply not constructed in headless mode. The InApp
//! approval sink is the exception: it speaks UiBridge and is constructed
//! whenever a WebView *or* a live event bus can deliver its questions.

use std::sync::{Arc, OnceLock};

use crate::state::AppState;

/// Set once by the headless composition root right after the fully
/// initialized `AppState` is wrapped in its final `Arc`. Background loops
/// started a moment earlier resolve state lazily (first sense event /
/// dispatch), so `wait()` in [`UiBridge::app_state`] only ever blocks for
/// the microseconds between loop spawn and publish.
static HEADLESS_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

/// Process-global fan-out of UI events to non-WebView consumers (the
/// HTTP API's SSE stream). Initialized only when the HTTP API is
/// enabled; when unset, `emit` skips the bus entirely. Lives here (not
/// in `http_api`) because `emit` is the single chokepoint every event
/// already flows through — publishing at the seam means zero call-site
/// changes and identical coverage in Tauri and headless modes.
static EVENT_BUS: OnceLock<tokio::sync::broadcast::Sender<BusEvent>> = OnceLock::new();

/// A UI event as seen by bus subscribers: the Tauri event name plus the
/// payload pre-serialized to JSON (SSE forwards it verbatim).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BusEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

/// Handle the autonomous core uses to reach the UI layer, if any.
#[derive(Clone)]
pub enum UiBridge {
    /// Desktop mode: a real Tauri app with a WebView frontend.
    Tauri(tauri::AppHandle),
    /// Daemon mode: no GUI; Telegram is the user surface.
    Headless,
}

impl UiBridge {
    /// Publish the headless `AppState` singleton. Must be called exactly
    /// once, by the headless composition root only.
    pub fn publish_headless_state(state: Arc<AppState>) {
        if HEADLESS_STATE.set(state).is_err() {
            tracing::error!("publish_headless_state called twice; keeping the first");
        }
    }

    /// Initialize the event bus so subsequent `emit` calls fan out to
    /// bus subscribers (the HTTP API's SSE stream). Idempotent; called
    /// by whichever composition root enables the HTTP API, before any
    /// background loop starts emitting.
    pub fn init_event_bus() {
        let _ = EVENT_BUS.get_or_init(|| tokio::sync::broadcast::channel(1024).0);
    }

    /// Subscribe to the event bus. `None` until [`Self::init_event_bus`]
    /// has run (i.e. the HTTP API is disabled).
    pub fn subscribe_events() -> Option<tokio::sync::broadcast::Receiver<BusEvent>> {
        EVENT_BUS.get().map(|tx| tx.subscribe())
    }

    /// Whether the event bus is live — i.e. a remote UI (HTTP API
    /// client) can receive events even without a Tauri window. Gates
    /// construction of the InApp approval sink in headless mode.
    pub fn event_bus_active() -> bool {
        EVENT_BUS.get().is_some()
    }

    /// Emit a frontend event. Forwards to the WebView in Tauri mode and
    /// to the HTTP event bus (when initialized) in both modes; with no
    /// GUI and no bus the event is dropped (logged at DEBUG).
    pub fn emit<S: serde::Serialize + Clone>(&self, event: &str, payload: S) {
        if let Some(tx) = EVENT_BUS.get() {
            if tx.receiver_count() > 0 {
                match serde_json::to_value(payload.clone()) {
                    Ok(v) => {
                        let _ = tx.send(BusEvent {
                            event: event.to_string(),
                            payload: v,
                        });
                    }
                    Err(e) => tracing::warn!(event, "event bus serialize failed: {e}"),
                }
            }
        }
        match self {
            UiBridge::Tauri(h) => {
                use tauri::Emitter;
                let _ = h.emit(event, payload);
            }
            UiBridge::Headless => {
                tracing::debug!(event, "ui event dropped (headless, no subscriber)");
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
