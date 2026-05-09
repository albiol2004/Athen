//! `LoggingWakeupSink`: Phase 3a placeholder sink. Writes a `SystemEvent`
//! arc entry describing the fire and emits a tracing event. The real
//! coordinator-backed sink (Phase 3b) replaces this, turning fires into
//! Tasks dispatched through the existing executor path.
//!
//! Why ship a placeholder first: Phase 3a's goal is the smoke test
//! (scheduler running in-app, store wired, Tauri commands authoring rows,
//! laptop fires the row). Coordinator dispatch needs allowlists +
//! `AutonomyBand` wiring + risk-gate plumbing — too much to land in one
//! pass without a working end-to-end loop to verify against.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

use athen_core::error::{AthenError, Result};
use athen_core::traits::wakeup::WakeupFireSink;
use athen_core::wakeup::{Wakeup, WakeupOrigin};
use athen_persistence::arcs::{ArcSource, ArcStore, EntryType};

/// Logs every fire and persists a `SystemEvent` arc entry into the target
/// arc (creating one if the wake-up has no `arc_id`).
pub struct LoggingWakeupSink {
    arc_store: Option<ArcStore>,
}

impl LoggingWakeupSink {
    pub fn new(arc_store: Option<ArcStore>) -> Self {
        Self { arc_store }
    }
}

#[async_trait]
impl WakeupFireSink for LoggingWakeupSink {
    async fn fire(&self, wakeup: &Wakeup, fired_at: DateTime<Utc>) -> Result<()> {
        info!(
            wakeup_id = %wakeup.id,
            instruction = %wakeup.instruction,
            profile = %wakeup.profile,
            origin = ?wakeup.origin,
            fired_at = %fired_at,
            "Wakeup fired (logging sink — Phase 3a, no dispatch yet)"
        );

        let Some(store) = self.arc_store.as_ref() else {
            // No arc store: log-only is the whole behavior.
            return Ok(());
        };

        // Resolve target arc: append to an existing one when the wake-up
        // declared `arc_id`; otherwise spawn a fresh arc named after the
        // wake-up so the user can find it in the arcs list.
        let target_arc_id = match &wakeup.arc_id {
            Some(id) => {
                // Validate the arc still exists; if not, fall through to
                // a fresh arc rather than silently dropping the entry.
                match store.get_arc(id).await {
                    Ok(Some(_)) => id.clone(),
                    Ok(None) => {
                        warn!(
                            wakeup_id = %wakeup.id,
                            missing_arc_id = %id,
                            "Wake-up's target arc no longer exists; creating a fresh arc"
                        );
                        create_fresh_arc_for_wakeup(store, wakeup).await?
                    }
                    Err(e) => {
                        warn!(
                            wakeup_id = %wakeup.id,
                            error = %e,
                            "Failed to look up wake-up arc; creating a fresh arc"
                        );
                        create_fresh_arc_for_wakeup(store, wakeup).await?
                    }
                }
            }
            None => create_fresh_arc_for_wakeup(store, wakeup).await?,
        };

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
        });
        let content = format!(
            "Wake-up fired: {}\n\n(Phase 3a placeholder — logging sink. Coordinator dispatch ships in Phase 3b.)",
            wakeup.instruction
        );
        store
            .add_entry(
                &target_arc_id,
                EntryType::SystemEvent,
                "wakeup",
                &content,
                Some(metadata),
                None,
            )
            .await
            .map(|_| ())
    }
}

/// Generate a fresh arc id and create the arc. Returns the new arc id.
async fn create_fresh_arc_for_wakeup(store: &ArcStore, wakeup: &Wakeup) -> Result<String> {
    let new_id = crate::sense_router::generate_arc_id();
    // Truncate the instruction for the arc name; full text lives in the
    // first entry so nothing is lost.
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
