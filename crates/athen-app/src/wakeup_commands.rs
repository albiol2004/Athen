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
use athen_core::contact::ContactId;
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
    /// Tool names the wake-up may invoke. `null` = use profile defaults.
    pub tool_allowlist: Option<Vec<String>>,
    /// Contact UUIDs (as strings) allowed as outbound recipients.
    pub contact_allowlist: Option<Vec<String>>,
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
            tool_allowlist: w.tool_allowlist.clone(),
            contact_allowlist: w
                .contact_allowlist
                .as_ref()
                .map(|v| v.iter().map(|c| c.to_string()).collect()),
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
    /// Optional tool name allowlist. `None` / empty = profile defaults.
    /// When `Some(non_empty)`, the wake-up registry hides every other tool
    /// from the agent's surface and refuses calls outside the list.
    pub tool_allowlist: Option<Vec<String>>,
    /// Optional contact id allowlist. `None` / empty = profile defaults.
    /// When `Some(non_empty)`, outbound tools (today: `email_send`) only
    /// accept recipients whose identifiers belong to one of these contacts.
    pub contact_allowlist: Option<Vec<String>>,
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

    let tool_allowlist = sanitize_tool_allowlist(req.tool_allowlist);
    let contact_allowlist = parse_contact_allowlist(req.contact_allowlist)?;

    let w = Wakeup {
        id: Uuid::new_v4(),
        schedule,
        instruction: req.instruction.trim().to_string(),
        autonomy,
        preferred_channel,
        tool_allowlist,
        contact_allowlist,
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

    let tool_allowlist = sanitize_tool_allowlist(req.tool_allowlist);
    let contact_allowlist = parse_contact_allowlist(req.contact_allowlist)?;

    // Preserve identity, origin, created_at, last_fired_at, enabled.
    let updated = Wakeup {
        id: existing.id,
        schedule,
        instruction,
        autonomy,
        preferred_channel,
        tool_allowlist,
        contact_allowlist,
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

/// Treat empty / whitespace-only entries as "no allowlist". The frontend
/// posts `[]` when the user clears the multiselect — that should mean
/// "use profile defaults," not "block every tool."
fn sanitize_tool_allowlist(v: Option<Vec<String>>) -> Option<Vec<String>> {
    let cleaned: Vec<String> = v
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn parse_contact_allowlist(
    v: Option<Vec<String>>,
) -> std::result::Result<Option<Vec<ContactId>>, String> {
    let raw = v.unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let id = Uuid::parse_str(s).map_err(|e| format!("Invalid contact id '{s}': {e}"))?;
        out.push(id);
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// One entry in the tool inventory the wake-up form renders.
#[derive(Debug, Serialize)]
pub struct ToolInventoryItem {
    /// Internal tool id — what `tool_allowlist` actually stores. The UI
    /// posts this back; humans never see it directly.
    pub name: String,
    /// Human-friendly label for the row ("Read file", "Send email").
    /// Falls back to a tidied form of `name` for tools we don't know
    /// about yet (e.g. third-party MCPs).
    pub display_name: String,
    /// Coarse category for grouping ("Filesystem", "Shell", "Web", etc.).
    /// MCP tools default to "MCP: <server-id>" so each external server
    /// gets its own collapsible section.
    pub category: String,
    pub description: String,
    /// Hint for the UI: outbound tools get a small "sends" badge so the
    /// user knows which ones are actually network-/world-affecting.
    pub outbound: bool,
}

/// Friendly label + category for the well-known built-in tools. The
/// wake-up form groups by category and shows the label instead of the
/// raw id so a non-technical user can pick `Send email` rather than
/// `email_send`. Tools missing from this table fall through to a
/// best-effort tidy of the underlying name.
fn tool_metadata(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        // Filesystem (built-in, gated by FileGate)
        "read" => ("Read file", "Filesystem"),
        "edit" => ("Edit file", "Filesystem"),
        "write" => ("Write file", "Filesystem"),
        "grep" => ("Search file contents", "Filesystem"),
        "list_directory" => ("List directory", "Filesystem"),
        // Shell
        "shell_execute" => ("Run shell command", "Shell"),
        "shell_spawn" => ("Spawn long-running process", "Shell"),
        "shell_kill" => ("Kill spawned process", "Shell"),
        "shell_logs" => ("Read process logs", "Shell"),
        // Memory
        "memory_store" => ("Save to memory", "Memory"),
        "memory_recall" => ("Recall from memory", "Memory"),
        // Web
        "web_search" => ("Search the web", "Web"),
        "web_fetch" => ("Fetch a web page", "Web"),
        // Email
        "email_send" => ("Send email", "Email"),
        // Calendar
        "calendar_list" => ("List calendar events", "Calendar"),
        "calendar_create" => ("Create calendar event", "Calendar"),
        "calendar_update" => ("Update calendar event", "Calendar"),
        "calendar_delete" => ("Delete calendar event", "Calendar"),
        // Contacts
        "contacts_list" => ("List contacts", "Contacts"),
        "contacts_search" => ("Search contacts", "Contacts"),
        "contacts_create" => ("Create contact", "Contacts"),
        "contacts_update" => ("Update contact", "Contacts"),
        "contacts_delete" => ("Delete contact", "Contacts"),
        // Attachments
        "read_attachment_full" => ("Read attachment text", "Attachments"),
        "fetch_attachment" => ("Fetch attachment bytes", "Attachments"),
        // Toolbox / packages
        "install_package" => ("Install package", "Toolbox"),
        "list_installed_packages" => ("List installed packages", "Toolbox"),
        "uninstall_package" => ("Uninstall package", "Toolbox"),
        // Delegation
        "delegate_to_agent" => ("Delegate to specialist agent", "Delegation"),
        _ => return None,
    })
}

/// Default-prettify an unknown tool id into a display label. Replaces
/// underscores with spaces, capitalizes the first letter — readable
/// enough for MCP-provided tools without forcing a full mapping.
fn humanize(name: &str) -> String {
    let cleaned = name.replace('_', " ");
    let mut chars = cleaned.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Snapshot of the tools available to a wake-up at *create* time. Built
/// against the same registry composition the executor uses, so what the
/// user sees in the multi-select matches what the agent can actually
/// invoke at fire time. Wake-ups don't have an arc id at create time —
/// we use the active arc when there is one (so MCP/file gates resolve)
/// and a placeholder otherwise. Outbound is a name-based heuristic that
/// matches the list in `wakeup_registry::OUTBOUND_TOOL_NAMES`.
///
/// Each entry carries a human-readable `display_name` + `category` so
/// the UI can render grouped collapsible sections instead of a flat
/// alphabetical wall of internal tool ids. The MCP filesystem tools
/// (`<mcp>__read_file`, `__write_file`, `__list_dir`, etc.) are dropped
/// from this list because the gated built-ins (`read`/`write`/`edit`/
/// `grep`/`list_directory`) cover the same ground and run through the
/// permission gate; exposing both would just bloat the picker.
#[tauri::command]
pub async fn list_available_tools(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<ToolInventoryItem>, String> {
    let arc_id_opt = state.active_arc_id.try_lock().map(|g| g.clone()).ok();
    let arc_id = arc_id_opt.unwrap_or_else(|| "wakeup-tool-inventory".to_string());
    let registry = state.build_tool_registry(&arc_id, None).await;
    let tools = registry
        .list_tools()
        .await
        .map_err(|e| format!("List tools: {e}"))?;
    let outbound: std::collections::HashSet<&str> = ["email_send"].into_iter().collect();

    // The filesystem MCP exposes `<mcp_id>__read_file`, `__write_file`,
    // `__list_dir`, `__create_dir`, `__delete`, etc. Built-ins cover the
    // same ground and route through the file-permission gate, so listing
    // both confuses the picker. Hide the MCP duplicates here; users who
    // genuinely need a non-default MCP filesystem can still allow the
    // server-prefixed name by typing it (future: an "advanced" toggle).
    const MCP_FILE_DUPLICATES: &[&str] = &[
        "read_file",
        "write_file",
        "append_file",
        "list_dir",
        "list_directory",
        "create_dir",
        "create_directory",
        "delete",
        "delete_file",
        "remove",
        "remove_file",
        "stat",
        "exists",
    ];

    let mut out: Vec<ToolInventoryItem> = tools
        .into_iter()
        .filter(|t| {
            // Drop filesystem MCP duplicates — keep MCPs that bring new
            // capability (Slack, Notion, etc.).
            if let Some((_, suffix)) = t.name.split_once("__") {
                if MCP_FILE_DUPLICATES.contains(&suffix) {
                    return false;
                }
            }
            true
        })
        .map(|t| {
            let (display_name, category) = match tool_metadata(&t.name) {
                Some((label, cat)) => (label.to_string(), cat.to_string()),
                None => match t.name.split_once("__") {
                    Some((mcp_id, suffix)) => (humanize(suffix), format!("MCP: {mcp_id}")),
                    None => (humanize(&t.name), "Other".to_string()),
                },
            };
            ToolInventoryItem {
                outbound: outbound.contains(t.name.as_str()),
                name: t.name,
                display_name,
                category,
                description: t.description,
            }
        })
        .collect();
    // Sort by category then display_name so the JS can group adjacent
    // entries without re-sorting.
    out.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    Ok(out)
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
