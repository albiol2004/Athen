//! Coordinator-backed wake-up fire sink.
//!
//! Each fire builds a synthetic [`SenseEvent`] with
//! `EventSource::System`, hands it to the coordinator (so risk
//! evaluation runs against the actual instruction at fire time),
//! registers the resulting task → arc mapping, and pokes the
//! dispatch loop. The agent then picks the task up like any other
//! autonomous work.
//!
//! Intentionally stops short of `AutonomyBand` + tool/contact
//! allowlists — those land in Phase 3c on top of this.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tauri::{AppHandle, Emitter};
use tracing::{info, warn};
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::risk::{RiskDecision, RiskLevel};
use athen_core::traits::wakeup::WakeupFireSink;
use athen_core::wakeup::{Wakeup, WakeupOrigin};
use athen_persistence::arcs::{ArcSource, ArcStore, EntryType};

use crate::state::{TaskArcMap, TaskWakeupMap};

/// Coordinator-backed sink. Each fire becomes a synthetic sense event
/// processed by the coordinator, and (on `SilentApprove` /
/// `NotifyAndProceed`) the resulting task is registered with the
/// dispatch loop so the agent runs unattended.
pub struct CoordinatorWakeupSink {
    coordinator: Arc<athen_coordinador::Coordinator>,
    arc_store: Option<ArcStore>,
    task_arc_map: TaskArcMap,
    task_wakeup_map: TaskWakeupMap,
    dispatch_signal: Arc<tokio::sync::Notify>,
    /// `None` in tests / non-Tauri builds. When `Some`, the sink emits a
    /// `wakeup-fired` event so the frontend can refresh its arc list and
    /// surface a toast.
    app_handle: Option<AppHandle>,
}

impl CoordinatorWakeupSink {
    pub fn new(
        coordinator: Arc<athen_coordinador::Coordinator>,
        arc_store: Option<ArcStore>,
        task_arc_map: TaskArcMap,
        task_wakeup_map: TaskWakeupMap,
        dispatch_signal: Arc<tokio::sync::Notify>,
        app_handle: Option<AppHandle>,
    ) -> Self {
        Self {
            coordinator,
            arc_store,
            task_arc_map,
            task_wakeup_map,
            dispatch_signal,
            app_handle,
        }
    }
}

#[async_trait]
impl WakeupFireSink for CoordinatorWakeupSink {
    async fn fire(&self, wakeup: &Wakeup, fired_at: DateTime<Utc>) -> Result<()> {
        info!(
            wakeup_id = %wakeup.id,
            instruction = %wakeup.instruction,
            profile = %wakeup.profile,
            origin = ?wakeup.origin,
            fired_at = %fired_at,
            "Wakeup fired — dispatching to coordinator"
        );

        // Resolve the arc up-front so the agent has a place to write
        // even if coordinator dispatch errors out.
        let target_arc_id = if let Some(store) = self.arc_store.as_ref() {
            let id = resolve_target_arc(store, wakeup).await?;
            info!(
                wakeup_id = %wakeup.id,
                arc_id = %id,
                declared = ?wakeup.arc_id,
                "Wake-up resolved target arc"
            );
            Some(id)
        } else {
            warn!(wakeup_id = %wakeup.id, "Sink has no arc_store — running headless");
            None
        };

        // Build a synthetic sense event. The coordinator's default router
        // maps `EventSource::System` to `DomainType::Base` /
        // `TaskPriority::Low`, with the wake-up instruction as the task
        // description — exactly what the executor consumes via
        // `task.description`.
        let event = SenseEvent {
            id: Uuid::new_v4(),
            timestamp: fired_at,
            source: EventSource::System,
            kind: EventKind::NewMessage,
            sender: None,
            content: NormalizedContent {
                summary: Some(wakeup.instruction.clone()),
                body: serde_json::json!({
                    "wakeup_id": wakeup.id.to_string(),
                    "fired_at": fired_at.to_rfc3339(),
                    "origin": match &wakeup.origin {
                        WakeupOrigin::User => "user".to_string(),
                        WakeupOrigin::Agent { authoring_arc_id } => {
                            format!("agent:{authoring_arc_id}")
                        }
                    },
                    "autonomy": wakeup.autonomy.as_str(),
                    "instruction": wakeup.instruction,
                }),
                attachments: Vec::new(),
            },
            source_risk: RiskLevel::Safe,
            raw_id: None,
        };

        let decisions = match self.coordinator.process_event(event).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    wakeup_id = %wakeup.id,
                    error = %e,
                    "Coordinator rejected wake-up event"
                );
                if let (Some(store), Some(arc_id)) = (self.arc_store.as_ref(), &target_arc_id) {
                    persist_fire_marker(
                        store,
                        arc_id,
                        wakeup,
                        fired_at,
                        Some(&format!("Coordinator error: {e}")),
                    )
                    .await
                    .ok();
                }
                return Err(e);
            }
        };

        // Persist a single "wake-up fired" marker per arc — written
        // before mapping the task so the user sees the trigger even if
        // the dispatch loop is busy. The decision label gets folded in
        // for transparency.
        let decision_label = decisions
            .first()
            .map(|(_, d)| decision_label(d))
            .unwrap_or("no_decision");
        if let (Some(store), Some(arc_id)) = (self.arc_store.as_ref(), &target_arc_id) {
            if let Err(e) =
                persist_fire_marker(store, arc_id, wakeup, fired_at, Some(decision_label)).await
            {
                warn!(
                    wakeup_id = %wakeup.id,
                    arc = %arc_id,
                    error = %e,
                    "Failed to persist wake-up fire marker"
                );
            }
        }

        // Hook each created task into the dispatch loop. Only
        // SilentApprove / NotifyAndProceed actually enqueue (and so
        // run); HumanConfirm goes to awaiting_approval and HardBlock
        // is dropped on the floor — both are surfaced via the marker
        // above. AutonomyBand-aware behavior lands in Phase 3c.
        let mut any_dispatched = false;
        for (task_id, decision) in &decisions {
            match decision {
                RiskDecision::SilentApprove | RiskDecision::NotifyAndProceed => {
                    if let Some(arc_id) = target_arc_id.clone() {
                        self.task_arc_map.write().await.insert(*task_id, arc_id);
                    }
                    self.task_wakeup_map
                        .write()
                        .await
                        .insert(*task_id, wakeup.id);
                    any_dispatched = true;
                }
                RiskDecision::HumanConfirm => {
                    // Register the arc anyway so that — once Phase 3c
                    // wires the approval router and the user approves —
                    // the dispatch loop knows where to write replies.
                    if let Some(arc_id) = target_arc_id.clone() {
                        self.task_arc_map.write().await.insert(*task_id, arc_id);
                    }
                    self.task_wakeup_map
                        .write()
                        .await
                        .insert(*task_id, wakeup.id);
                    info!(
                        wakeup_id = %wakeup.id,
                        task_id = %task_id,
                        "Wake-up requires human confirmation; queued for approval"
                    );
                }
                RiskDecision::HardBlock => {
                    warn!(
                        wakeup_id = %wakeup.id,
                        task_id = %task_id,
                        "Wake-up hard-blocked at coordinator risk gate"
                    );
                }
            }
        }

        if any_dispatched {
            self.dispatch_signal.notify_one();
        }

        // Tell the frontend a wake-up just fired so it can:
        //   - refresh its arc list (so a freshly-created wake-up arc
        //     shows up in the sidebar without an app restart),
        //   - surface a toast / sense-event-style notification linking
        //     to the arc.
        // Best-effort: emit failures don't invalidate the fire.
        if let Some(app) = &self.app_handle {
            let payload = serde_json::json!({
                "wakeup_id": wakeup.id.to_string(),
                "arc_id": target_arc_id,
                "instruction": wakeup.instruction,
                "fired_at": fired_at.to_rfc3339(),
                "decision": decision_label,
                "autonomy": wakeup.autonomy.as_str(),
            });
            match app.emit("wakeup-fired", payload) {
                Ok(()) => info!(
                    wakeup_id = %wakeup.id,
                    arc_id = ?target_arc_id,
                    decision = %decision_label,
                    "wakeup-fired event emitted to frontend"
                ),
                Err(e) => {
                    warn!(wakeup_id = %wakeup.id, error = %e, "Failed to emit wakeup-fired event")
                }
            }
        } else {
            info!(wakeup_id = %wakeup.id, "Sink has no AppHandle; not emitting wakeup-fired");
        }

        Ok(())
    }
}

/// Resolve the arc to write into. Existing arc when the wake-up declared
/// `arc_id` and it's still alive; otherwise spawn a fresh arc named after
/// the wake-up's instruction.
async fn resolve_target_arc(store: &ArcStore, wakeup: &Wakeup) -> Result<String> {
    match &wakeup.arc_id {
        Some(id) => match store.get_arc(id).await {
            Ok(Some(_)) => Ok(id.clone()),
            Ok(None) => {
                warn!(
                    wakeup_id = %wakeup.id,
                    missing_arc_id = %id,
                    "Wake-up's target arc no longer exists; creating a fresh arc"
                );
                create_fresh_arc_for_wakeup(store, wakeup).await
            }
            Err(e) => {
                warn!(
                    wakeup_id = %wakeup.id,
                    error = %e,
                    "Failed to look up wake-up arc; creating a fresh arc"
                );
                create_fresh_arc_for_wakeup(store, wakeup).await
            }
        },
        None => create_fresh_arc_for_wakeup(store, wakeup).await,
    }
}

/// Persist the "wake-up fired" SystemEvent entry. `note` is appended to
/// the body (decision label, error message, etc.).
async fn persist_fire_marker(
    store: &ArcStore,
    arc_id: &str,
    wakeup: &Wakeup,
    fired_at: DateTime<Utc>,
    note: Option<&str>,
) -> Result<()> {
    let metadata = serde_json::json!({
        "wakeup_id": wakeup.id.to_string(),
        "fired_at": fired_at.to_rfc3339(),
        "origin": match &wakeup.origin {
            WakeupOrigin::User => "user".to_string(),
            WakeupOrigin::Agent { authoring_arc_id } => {
                format!("agent:{authoring_arc_id}")
            }
        },
        "instruction": wakeup.instruction,
        "decision": note.unwrap_or(""),
    });
    let content = match note {
        Some(extra) => format!("Wake-up fired: {}\n\n[risk: {}]", wakeup.instruction, extra),
        None => format!("Wake-up fired: {}", wakeup.instruction),
    };
    store
        .add_entry(
            arc_id,
            EntryType::SystemEvent,
            "wakeup",
            &content,
            Some(metadata),
            None,
        )
        .await
        .map(|_| ())
}

/// Generate a fresh arc id and create the arc. Returns the new arc id.
async fn create_fresh_arc_for_wakeup(store: &ArcStore, wakeup: &Wakeup) -> Result<String> {
    let new_id = crate::sense_router::generate_arc_id();
    let name_seed: String = wakeup.instruction.chars().take(48).collect();
    let arc_name = if name_seed.trim().is_empty() {
        format!("Wake-up {}", &wakeup.id.to_string()[..8])
    } else {
        format!("Wake-up: {name_seed}")
    };
    store
        .create_arc(&new_id, &arc_name, ArcSource::System)
        .await
        .map_err(|e| AthenError::Other(format!("Create arc for wakeup: {e}")))?;
    Ok(new_id)
}

fn decision_label(decision: &RiskDecision) -> &'static str {
    match decision {
        RiskDecision::SilentApprove => "silent_approve",
        RiskDecision::NotifyAndProceed => "notify_and_proceed",
        RiskDecision::HumanConfirm => "human_confirm",
        RiskDecision::HardBlock => "hard_block",
    }
}
