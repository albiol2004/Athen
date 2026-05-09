//! Tauri commands for wake-ups (Phase 3a smoke-test surface).
//!
//! Minimal: create one-shot or interval, list, delete, toggle. Cron
//! authoring + UI tab arrive in Phase 4.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use athen_core::config::NotificationChannelKind;
use athen_core::traits::wakeup::WakeupStore;
use athen_core::wakeup::{AutonomyBand, Schedule, Wakeup, WakeupOrigin};

use crate::state::AppState;

/// Frontend-shaped wake-up. Mirrors `Wakeup` but stringifies enums and
/// timestamps for ergonomic JS consumption.
#[derive(Debug, Serialize)]
pub struct WakeupView {
    pub id: String,
    pub instruction: String,
    pub schedule_kind: String,    // "one_shot" | "cron" | "interval"
    pub schedule_summary: String, // human-readable line for the row
    pub next_fire_at: Option<String>,
    pub last_fired_at: Option<String>,
    pub created_at: String,
    pub enabled: bool,
    pub autonomy: String,
    pub origin: String, // "user" | "agent"
    pub arc_id: Option<String>,
    pub profile: String,
    pub preferred_channel: Option<String>,
}

impl From<&Wakeup> for WakeupView {
    fn from(w: &Wakeup) -> Self {
        let (kind, summary) = match &w.schedule {
            Schedule::OneShot { at } => ("one_shot", format!("Once at {}", at.to_rfc3339())),
            Schedule::Cron { expr, tz } => ("cron", format!("Cron `{expr}` ({tz})")),
            Schedule::Interval {
                every_seconds,
                anchor,
            } => (
                "interval",
                format!("Every {every_seconds}s from {}", anchor.to_rfc3339()),
            ),
        };
        WakeupView {
            id: w.id.to_string(),
            instruction: w.instruction.clone(),
            schedule_kind: kind.into(),
            schedule_summary: summary,
            next_fire_at: w.next_fire_at.map(|d| d.to_rfc3339()),
            last_fired_at: w.last_fired_at.map(|d| d.to_rfc3339()),
            created_at: w.created_at.to_rfc3339(),
            enabled: w.enabled,
            autonomy: w.autonomy.as_str().into(),
            origin: match &w.origin {
                WakeupOrigin::User => "user".into(),
                WakeupOrigin::Agent { .. } => "agent".into(),
            },
            arc_id: w.arc_id.clone(),
            profile: w.profile.clone(),
            preferred_channel: w.preferred_channel.as_ref().map(|c| match c {
                NotificationChannelKind::InApp => "in_app".to_string(),
                NotificationChannelKind::Telegram => "telegram".to_string(),
            }),
        }
    }
}

/// Frontend payload for `create_wakeup`. Schedule is one of three shapes;
/// caller fills the relevant fields.
#[derive(Debug, Deserialize)]
pub struct CreateWakeupReq {
    pub instruction: String,
    pub schedule: ScheduleReq,
    /// Defaults to "assistant" if omitted.
    pub profile: Option<String>,
    /// Defaults to `safe_only` if omitted.
    pub autonomy: Option<String>,
    pub arc_id: Option<String>,
    /// "in_app" | "telegram"; null = use system default at notify time.
    pub preferred_channel: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleReq {
    OneShot {
        at: String,
    },
    Cron {
        expr: String,
        tz: String,
    },
    Interval {
        every_seconds: u64,
        anchor: Option<String>,
    },
}

impl ScheduleReq {
    fn into_schedule(self, now: DateTime<Utc>) -> std::result::Result<Schedule, String> {
        match self {
            ScheduleReq::OneShot { at } => parse_dt(&at).map(|at| Schedule::OneShot { at }),
            ScheduleReq::Cron { expr, tz } => Ok(Schedule::Cron { expr, tz }),
            ScheduleReq::Interval {
                every_seconds,
                anchor,
            } => {
                let anchor = match anchor {
                    Some(s) => parse_dt(&s)?,
                    None => now,
                };
                if every_seconds == 0 {
                    return Err("every_seconds must be > 0".into());
                }
                Ok(Schedule::Interval {
                    every_seconds,
                    anchor,
                })
            }
        }
    }
}

fn parse_dt(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| format!("Invalid datetime '{s}': {e}"))
}

fn parse_channel(s: &str) -> std::result::Result<NotificationChannelKind, String> {
    match s {
        "in_app" | "InApp" => Ok(NotificationChannelKind::InApp),
        "telegram" | "Telegram" => Ok(NotificationChannelKind::Telegram),
        other => Err(format!("Unknown notification channel '{other}'")),
    }
}

#[tauri::command]
pub async fn create_wakeup(
    req: CreateWakeupReq,
    state: State<'_, AppState>,
) -> std::result::Result<WakeupView, String> {
    let store = state
        .wakeup_store
        .clone()
        .ok_or_else(|| "Wake-up store not initialized".to_string())?;

    let now = Utc::now();
    let schedule = req.schedule.into_schedule(now)?;
    // Compute the initial next_fire_at so the first tick can pick the row
    // up without a separate `arm_unscheduled` round trip.
    let next_fire_at = athen_scheduler::compute_next_fire(&schedule, now);
    if next_fire_at.is_none() {
        return Err(
            "Schedule produced no next fire time (one-shot in the past or invalid cron)"
                .to_string(),
        );
    }

    let preferred_channel = match req.preferred_channel.as_deref() {
        Some(s) => Some(parse_channel(s)?),
        None => None,
    };
    let autonomy = req
        .autonomy
        .as_deref()
        .map(AutonomyBand::from_str_lossy)
        .unwrap_or(AutonomyBand::SafeOnly);

    let w = Wakeup {
        id: Uuid::new_v4(),
        schedule,
        instruction: req.instruction.trim().to_string(),
        autonomy,
        preferred_channel,
        tool_allowlist: None,
        contact_allowlist: None,
        profile: req.profile.unwrap_or_else(|| "assistant".to_string()),
        arc_id: req.arc_id,
        origin: WakeupOrigin::User,
        created_at: now,
        last_fired_at: None,
        next_fire_at,
        enabled: true,
    };
    if w.instruction.is_empty() {
        return Err("Instruction cannot be empty".into());
    }
    store
        .create(&w)
        .await
        .map_err(|e| format!("Create wakeup: {e}"))?;
    Ok(WakeupView::from(&w))
}

/// Patch an existing wake-up. Same shape as `CreateWakeupReq` — every
/// editable field gets replaced. `next_fire_at` is recomputed from the
/// (possibly new) schedule, `last_fired_at` is preserved, `enabled` is
/// preserved (toggle via `set_wakeup_enabled`).
#[tauri::command]
pub async fn update_wakeup(
    id: String,
    req: CreateWakeupReq,
    state: State<'_, AppState>,
) -> std::result::Result<WakeupView, String> {
    let store = state
        .wakeup_store
        .clone()
        .ok_or_else(|| "Wake-up store not initialized".to_string())?;

    let id = Uuid::parse_str(&id).map_err(|e| format!("Invalid id: {e}"))?;
    let existing = store
        .get(id)
        .await
        .map_err(|e| format!("Lookup wakeup: {e}"))?
        .ok_or_else(|| format!("Wake-up {id} not found"))?;

    let now = Utc::now();
    let schedule = req.schedule.into_schedule(now)?;
    let next_fire_at = athen_scheduler::compute_next_fire(&schedule, now);
    if next_fire_at.is_none() {
        return Err(
            "Schedule produced no next fire time (one-shot in the past or invalid cron)"
                .to_string(),
        );
    }

    let preferred_channel = match req.preferred_channel.as_deref() {
        Some(s) => Some(parse_channel(s)?),
        None => None,
    };
    let autonomy = req
        .autonomy
        .as_deref()
        .map(AutonomyBand::from_str_lossy)
        .unwrap_or(existing.autonomy);

    let instruction = req.instruction.trim().to_string();
    if instruction.is_empty() {
        return Err("Instruction cannot be empty".into());
    }

    // Preserve identity, origin, created_at, last_fired_at, enabled.
    // Allowlists aren't yet authorable from the form, so they survive
    // a round-trip from whatever they were on creation.
    let updated = Wakeup {
        id: existing.id,
        schedule,
        instruction,
        autonomy,
        preferred_channel,
        tool_allowlist: existing.tool_allowlist,
        contact_allowlist: existing.contact_allowlist,
        profile: req.profile.unwrap_or(existing.profile),
        arc_id: req.arc_id,
        origin: existing.origin,
        created_at: existing.created_at,
        last_fired_at: existing.last_fired_at,
        next_fire_at,
        enabled: existing.enabled,
    };
    store
        .update(&updated)
        .await
        .map_err(|e| format!("Update wakeup: {e}"))?;
    Ok(WakeupView::from(&updated))
}

#[tauri::command]
pub async fn list_wakeups(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<WakeupView>, String> {
    let store: Arc<_> = state
        .wakeup_store
        .clone()
        .ok_or_else(|| "Wake-up store not initialized".to_string())?;
    let rows = store
        .list_all()
        .await
        .map_err(|e| format!("List wakeups: {e}"))?;
    Ok(rows.iter().map(WakeupView::from).collect())
}

#[tauri::command]
pub async fn delete_wakeup(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let store = state
        .wakeup_store
        .clone()
        .ok_or_else(|| "Wake-up store not initialized".to_string())?;
    let id = Uuid::parse_str(&id).map_err(|e| format!("Invalid id: {e}"))?;
    store
        .delete(id)
        .await
        .map_err(|e| format!("Delete wakeup: {e}"))
}

#[tauri::command]
pub async fn set_wakeup_enabled(
    id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let store = state
        .wakeup_store
        .clone()
        .ok_or_else(|| "Wake-up store not initialized".to_string())?;
    let id = Uuid::parse_str(&id).map_err(|e| format!("Invalid id: {e}"))?;
    store
        .set_enabled(id, enabled)
        .await
        .map_err(|e| format!("Set enabled: {e}"))
}
