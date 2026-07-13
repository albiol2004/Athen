//! Coordinator-backed wake-up fire sink.
//!
//! Each fire builds a synthetic [`SenseEvent`] with
//! `EventSource::System`, hands it to the coordinator (so risk
//! evaluation runs against the actual instruction at fire time),
//! registers the resulting task → arc mapping, and pokes the
//! dispatch loop. The agent then picks the task up like any other
//! autonomous work.
//!
//! The wake-up's [`AutonomyBand`] is passed through
//! `Coordinator::process_event_authorized` so that pre-approved
//! triggers aren't hard-blocked by the coordinator's blanket gate;
//! per-action risk gating and tool/contact allowlists still apply at
//! execution time.

use std::sync::Arc;

use crate::ui_bridge::UiBridge;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
    /// `None` in tests. When `Some`, the sink resolves live security mode
    /// through the bridge and emits a `wakeup-fired` event (dropped in
    /// headless mode) so the frontend can refresh its arc list.
    ui: Option<UiBridge>,
}

impl CoordinatorWakeupSink {
    pub fn new(
        coordinator: Arc<athen_coordinador::Coordinator>,
        arc_store: Option<ArcStore>,
        task_arc_map: TaskArcMap,
        task_wakeup_map: TaskWakeupMap,
        dispatch_signal: Arc<tokio::sync::Notify>,
        ui: Option<UiBridge>,
    ) -> Self {
        Self {
            coordinator,
            arc_store,
            task_arc_map,
            task_wakeup_map,
            dispatch_signal,
            ui,
        }
    }

    /// Best-effort failure ping. Success already pings (completion
    /// notification in `execute_dispatched_task`); this makes every FIRE
    /// FAILURE equally visible so a wake-up that dies at the sink is no
    /// longer silent. Resolves the LIVE notifier through the bridge at
    /// fire time (like the security-mode lookup above) rather than a
    /// stale snapshot, so a settings-change swap is honoured. Silent
    /// no-op when there's no bridge (tests, `ui: None`) or no notifier
    /// installed. Never offers action buttons — the fire failed, so
    /// there's no action the system took to respond to.
    async fn notify_failure(&self, wakeup: &Wakeup, arc_id: Option<&str>, reason: &str) {
        let Some(notifier) = self
            .ui
            .as_ref()
            .and_then(|ui| ui.app_state().notifier.load_full())
        else {
            return;
        };
        // Prefer the arc's display name; fall back to the instruction snippet.
        let label = match (arc_id, self.arc_store.as_ref()) {
            (Some(id), Some(store)) => match store.get_arc(id).await {
                Ok(Some(meta)) => meta.name,
                _ => instruction_snippet(wakeup),
            },
            _ => instruction_snippet(wakeup),
        };
        let body = if reason.chars().count() > 160 {
            let cap = reason.floor_char_boundary(160);
            format!("{}...", &reason[..cap])
        } else {
            reason.to_string()
        };
        let notification = athen_core::notification::Notification {
            id: Uuid::new_v4(),
            // No `Normal` variant exists (Low/Medium/High/Critical);
            // Medium = "should reach user soon", the right weight for a
            // missed wake-up the user is waiting on.
            urgency: athen_core::notification::NotificationUrgency::Medium,
            title: format!("Athen couldn't finish: {label}"),
            body,
            origin: athen_core::notification::NotificationOrigin::Agent,
            arc_id: arc_id.map(|s| s.to_string()),
            task_id: None,
            created_at: Utc::now(),
            requires_response: false,
            skip_humanize: true,
            body_long: None,
        };
        notifier.notify(notification).await;
    }
}

/// First ~48 chars of the wake-up instruction, for a notification label
/// when no arc name is available.
fn instruction_snippet(wakeup: &Wakeup) -> String {
    let snippet: String = wakeup.instruction.chars().take(48).collect();
    if snippet.trim().is_empty() {
        format!("wake-up {}", &wakeup.id.to_string()[..8])
    } else {
        snippet
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
            match resolve_target_arc(store, wakeup).await {
                Ok(id) => {
                    info!(
                        wakeup_id = %wakeup.id,
                        arc_id = %id,
                        declared = ?wakeup.arc_id,
                        "Wake-up resolved target arc"
                    );
                    Some(id)
                }
                Err(e) => {
                    // Arc resolution itself failed (the fresh-arc create
                    // errored) — there is no arc to persist a marker into,
                    // so the ping is the only surface. Fire it before
                    // propagating so the miss isn't silent.
                    warn!(
                        wakeup_id = %wakeup.id,
                        error = %e,
                        "Failed to resolve wake-up target arc"
                    );
                    self.notify_failure(
                        wakeup,
                        None,
                        &format!("Couldn't prepare a workspace: {e}"),
                    )
                    .await;
                    return Err(e);
                }
            }
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

        // Effective security posture: per-arc override ⊕ live global
        // (Assistant in tests with no bridge). `target_arc_id` is `None`
        // for store-less fires — the resolver then returns the global.
        let global_security_mode = self
            .ui
            .as_ref()
            .map(|ui| ui.app_state().security.load().mode)
            .unwrap_or(athen_core::config::SecurityMode::Assistant);
        let security_mode = crate::state::resolve_security_mode_for_arc(
            self.arc_store.as_ref(),
            target_arc_id.as_deref().unwrap_or(""),
            global_security_mode,
        )
        .await;
        let decisions = match self
            .coordinator
            .process_event_authorized(event, wakeup.autonomy, security_mode)
            .await
        {
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
                self.notify_failure(
                    wakeup,
                    target_arc_id.as_deref(),
                    &format!("Coordinator error: {e}"),
                )
                .await;
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
        // above. The coordinator already applied the wake-up's
        // AutonomyBand to widen the decision before returning here.
        let mut any_dispatched = false;
        let mut any_confirm = false;
        let mut hard_blocked = false;
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
                    any_confirm = true;
                }
                RiskDecision::HardBlock => {
                    warn!(
                        wakeup_id = %wakeup.id,
                        task_id = %task_id,
                        "Wake-up hard-blocked at coordinator risk gate"
                    );
                    hard_blocked = true;
                }
            }
        }

        if any_dispatched {
            self.dispatch_signal.notify_one();
        }

        // HardBlock-only decision set: nothing ran and nothing is awaiting
        // approval, so this wake-up is silently consumed unless we ping.
        // (The autonomy-coercion rule means Auto-band wake-ups never see
        // HardBlock here — name the band so the user knows why.) HumanConfirm
        // is a legit awaiting-approval, not a failure, so it never pings.
        if hard_blocked && !any_dispatched && !any_confirm {
            self.notify_failure(
                wakeup,
                target_arc_id.as_deref(),
                &format!(
                    "Blocked by the risk gate (autonomy band: {}).",
                    wakeup.autonomy.as_str()
                ),
            )
            .await;
        }

        // Tell the frontend a wake-up just fired so it can:
        //   - refresh its arc list (so a freshly-created wake-up arc
        //     shows up in the sidebar without an app restart),
        //   - surface a toast / sense-event-style notification linking
        //     to the arc.
        // Best-effort: emit failures don't invalidate the fire.
        if let Some(ui) = &self.ui {
            let payload = serde_json::json!({
                "wakeup_id": wakeup.id.to_string(),
                "arc_id": target_arc_id,
                "instruction": wakeup.instruction,
                "fired_at": fired_at.to_rfc3339(),
                "decision": decision_label,
                "autonomy": wakeup.autonomy.as_str(),
            });
            ui.emit("wakeup-fired", payload);
            info!(
                wakeup_id = %wakeup.id,
                arc_id = ?target_arc_id,
                decision = %decision_label,
                "wakeup-fired event emitted"
            );
        } else {
            info!(wakeup_id = %wakeup.id, "Sink has no UiBridge; not emitting wakeup-fired");
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

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::wakeup::{AutonomyBand, Schedule};
    use rusqlite::Connection;
    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex;

    fn sample_wakeup(instruction: &str) -> Wakeup {
        Wakeup {
            id: Uuid::new_v4(),
            schedule: Schedule::OneShot { at: Utc::now() },
            instruction: instruction.to_string(),
            autonomy: AutonomyBand::Auto,
            preferred_channel: None,
            tool_allowlist: None,
            contact_allowlist: None,
            inherit_restrictions: true,
            profile: "assistant".to_string(),
            arc_id: None,
            origin: WakeupOrigin::User,
            created_at: Utc::now(),
            last_fired_at: None,
            next_fire_at: None,
            enabled: true,
        }
    }

    async fn in_memory_store() -> ArcStore {
        let conn = StdArc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let store = ArcStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    /// The coordinator-reject path persists a best-effort fire marker before
    /// returning `Err` (so the miss is at least visible in the arc even when
    /// there's no notifier). This exercises that marker write directly: a
    /// `SystemEvent` "wakeup" entry whose content folds in the error note.
    #[tokio::test]
    async fn persist_fire_marker_writes_system_event_with_note() {
        let store = in_memory_store().await;
        store
            .create_arc("arc_test", "Wake-up test", ArcSource::System)
            .await
            .unwrap();
        let wakeup = sample_wakeup("water the plants");
        let fired_at = Utc::now();

        persist_fire_marker(
            &store,
            "arc_test",
            &wakeup,
            fired_at,
            Some("Coordinator error: boom"),
        )
        .await
        .unwrap();

        let entries = store.load_entries("arc_test").await.unwrap();
        let marker = entries
            .iter()
            .find(|e| e.entry_type == EntryType::SystemEvent && e.source == "wakeup")
            .expect("a wakeup SystemEvent marker was persisted");
        assert!(marker.content.contains("water the plants"));
        assert!(marker.content.contains("Coordinator error: boom"));
    }

    /// The instruction snippet caps at 48 chars and falls back to the id when
    /// the instruction is blank — the notification label used when no arc name
    /// is available.
    #[test]
    fn instruction_snippet_caps_and_falls_back() {
        let mut wakeup = sample_wakeup(&"x".repeat(100));
        assert_eq!(instruction_snippet(&wakeup).chars().count(), 48);

        wakeup.instruction = "   ".to_string();
        assert!(instruction_snippet(&wakeup).starts_with("wake-up "));
    }
}
