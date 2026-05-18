//! Tauri commands behind the **Settings → Calendar Sources** panel.
//!
//! Provides the CRUD + probe + manual-sync surface the frontend needs:
//!
//! - `list_calendar_sources` — table data for the panel
//! - `add_caldav_source` — add a new CalDAV account (vault-stores password)
//! - `delete_calendar_source` — remove the row and its vault entry
//! - `set_calendar_source_enabled` — toggle without deleting
//! - `set_calendar_source_selected_calendars` — narrow the sync set
//! - `test_calendar_source_connection` — auth + discovery probe
//! - `list_remote_calendars` — for the "which calendars to sync?" picker
//! - `sync_calendar_source_now` — manual one-shot pull (returns counts)
//!
//! Heavy work (HTTP / vault / SQLite) is async and bounded by the
//! adapter's 30 s HTTP timeout. Errors are surfaced as `String`
//! (frontend pattern) — full details still hit `tracing::warn`.

use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use athen_core::calendar_source_config::CalendarSourceConfig;
use athen_core::traits::calendar_source::CalendarSource;
use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;

use crate::state::AppState;

/// View-model returned to the frontend. Mirrors `CalendarSourceConfig`
/// minus internal vault routing details (scope/key) the UI never needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarSourceView {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub base_url: String,
    pub username: String,
    pub enabled: bool,
    pub selected_calendars: Vec<String>,
    pub sync_interval_secs: u64,
    pub last_sync_at: Option<String>,
    pub last_sync_error: Option<String>,
    pub created_at: String,
}

impl From<CalendarSourceConfig> for CalendarSourceView {
    fn from(c: CalendarSourceConfig) -> Self {
        Self {
            id: c.id.to_string(),
            kind: c.kind.as_str().to_string(),
            display_name: c.display_name,
            base_url: c.base_url,
            username: c.username,
            enabled: c.enabled,
            selected_calendars: c.selected_calendars,
            sync_interval_secs: c.sync_interval_secs,
            last_sync_at: c.last_sync_at.map(|d| d.to_rfc3339()),
            last_sync_error: c.last_sync_error,
            created_at: c.created_at.to_rfc3339(),
        }
    }
}

/// One remote calendar collection, in the shape the picker dialog wants.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCalendarView {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub read_only: bool,
}

/// Manual-sync result the frontend can show as a toast.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub success: bool,
    pub message: String,
}

#[tauri::command]
pub async fn list_calendar_sources(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<CalendarSourceView>, String> {
    let store = source_store(&state)?;
    let configs = store.list().await.map_err(|e| e.to_string())?;
    Ok(configs.into_iter().map(CalendarSourceView::from).collect())
}

#[tauri::command]
pub async fn add_caldav_source(
    display_name: String,
    base_url: String,
    username: String,
    password: String,
    state: State<'_, AppState>,
) -> std::result::Result<CalendarSourceView, String> {
    if password.trim().is_empty() {
        return Err("Password required".to_string());
    }
    let vault = state
        .vault
        .clone()
        .ok_or_else(|| "Credential vault unavailable".to_string())?;
    let store = source_store(&state)?;

    let cfg = CalendarSourceConfig::new_caldav(display_name, base_url, username);
    vault
        .set(&cfg.vault_scope, &cfg.vault_key, password.trim())
        .await
        .map_err(|e| format!("Vault write failed: {e}"))?;
    if let Err(e) = store.upsert(&cfg).await {
        // Best-effort vault cleanup so an orphan secret doesn't linger
        // if the row write failed.
        let _ = vault.delete(&cfg.vault_scope, &cfg.vault_key).await;
        return Err(format!("Save calendar source: {e}"));
    }
    Ok(cfg.into())
}

#[tauri::command]
pub async fn delete_calendar_source(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Bad id: {e}"))?;
    let store = source_store(&state)?;
    // Read first to learn the vault scope so we can also delete the
    // password — order matters: row + vault both gone, no orphan secret.
    if let Some(existing) = store.get(uuid).await.map_err(|e| e.to_string())? {
        if let Some(vault) = state.vault.as_ref() {
            let _ = vault
                .delete(&existing.vault_scope, &existing.vault_key)
                .await;
        }
    }
    store.delete(uuid).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn set_calendar_source_enabled(
    id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Bad id: {e}"))?;
    source_store(&state)?
        .set_enabled(uuid, enabled)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_calendar_source_selected_calendars(
    id: String,
    calendar_ids: Vec<String>,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Bad id: {e}"))?;
    source_store(&state)?
        .set_selected_calendars(uuid, &calendar_ids)
        .await
        .map_err(|e| e.to_string())
}

/// Probe the remote: do auth + discovery succeed? Does NOT sync.
#[tauri::command]
pub async fn test_calendar_source_connection(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<SyncResult, String> {
    let source = build_live_source(&id, &state).await?;
    match source.test_connection().await {
        Ok(()) => Ok(SyncResult {
            success: true,
            message: "Connected and discovered calendar home.".to_string(),
        }),
        Err(e) => Ok(SyncResult {
            success: false,
            message: e.to_string(),
        }),
    }
}

/// Enumerate the calendars exposed by a configured source. Used by the
/// "pick which calendars to sync" dialog.
#[tauri::command]
pub async fn list_remote_calendars(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<RemoteCalendarView>, String> {
    let source = build_live_source(&id, &state).await?;
    let cals = source.list_calendars().await.map_err(|e| e.to_string())?;
    Ok(cals
        .into_iter()
        .map(|c| RemoteCalendarView {
            id: c.id,
            name: c.name,
            color: c.color,
            read_only: c.read_only,
        })
        .collect())
}

/// One-shot manual sync pass. Useful right after adding a source so the
/// user sees their events appear without waiting for the next interval.
#[tauri::command]
pub async fn sync_calendar_source_now(
    id: String,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> std::result::Result<SyncResult, String> {
    use std::sync::Arc as StdArc;

    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Bad id: {e}"))?;
    let store_concrete = source_store(&state)?;
    let cfg = store_concrete
        .get(uuid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Calendar source not found".to_string())?;
    let vault = state
        .vault
        .clone()
        .ok_or_else(|| "Credential vault unavailable".to_string())?;
    let calendar_store = state
        .calendar_store
        .clone()
        .ok_or_else(|| "Calendar store unavailable".to_string())?;
    let cfg_store: StdArc<dyn CalendarSourceConfigStore> = StdArc::new(store_concrete);

    match crate::calendar_sources::sync_one(&cfg, &vault, &calendar_store, &cfg_store).await {
        Ok(stats) => {
            emit_sync_completed(&app_handle, &cfg.id.to_string(), &cfg.display_name, stats);
            Ok(SyncResult {
                success: true,
                message: format!(
                    "Synced: +{} new, ~{} updated, -{} removed",
                    stats.inserted, stats.updated, stats.deleted
                ),
            })
        }
        Err(e) => Ok(SyncResult {
            success: false,
            message: e.to_string(),
        }),
    }
}

/// Sync every enabled source in one shot. Used by the Calendar view's
/// header "Sync" button so the user doesn't have to bounce to Settings.
/// Returns the aggregate counts; per-source errors are folded into
/// `errors` rather than aborting the whole call.
#[tauri::command]
pub async fn sync_all_calendar_sources_now(
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> std::result::Result<SyncAllResult, String> {
    use std::sync::Arc as StdArc;

    let store_concrete = source_store(&state)?;
    let sources = store_concrete.list().await.map_err(|e| e.to_string())?;
    let vault = state
        .vault
        .clone()
        .ok_or_else(|| "Credential vault unavailable".to_string())?;
    let calendar_store = state
        .calendar_store
        .clone()
        .ok_or_else(|| "Calendar store unavailable".to_string())?;
    let cfg_store: StdArc<dyn CalendarSourceConfigStore> = StdArc::new(store_concrete);

    let mut totals = SyncAllResult::default();
    for cfg in sources {
        if !cfg.enabled {
            continue;
        }
        totals.sources_tried += 1;
        match crate::calendar_sources::sync_one(&cfg, &vault, &calendar_store, &cfg_store).await {
            Ok(stats) => {
                totals.inserted += stats.inserted;
                totals.updated += stats.updated;
                totals.deleted += stats.deleted;
                emit_sync_completed(&app_handle, &cfg.id.to_string(), &cfg.display_name, stats);
            }
            Err(e) => {
                totals.errors.push(format!("{}: {}", cfg.display_name, e));
            }
        }
    }
    Ok(totals)
}

fn emit_sync_completed(
    handle: &tauri::AppHandle,
    source_id: &str,
    source_name: &str,
    stats: crate::calendar_sources::SyncStats,
) {
    use tauri::Emitter as _;
    let payload = serde_json::json!({
        "source_id": source_id,
        "source_name": source_name,
        "inserted": stats.inserted,
        "updated": stats.updated,
        "deleted": stats.deleted,
    });
    if let Err(e) = handle.emit("calendar-sync-completed", payload) {
        tracing::debug!(error = %e, "Failed to emit calendar-sync-completed");
    }
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncAllResult {
    pub sources_tried: usize,
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub errors: Vec<String>,
}

fn source_store(
    state: &AppState,
) -> std::result::Result<athen_persistence::calendar_sources::SqliteCalendarSourceStore, String> {
    state
        .calendar_source_store()
        .ok_or_else(|| "Database unavailable".to_string())
}

async fn build_live_source(
    id: &str,
    state: &AppState,
) -> std::result::Result<Box<dyn CalendarSource>, String> {
    let uuid = Uuid::parse_str(id).map_err(|e| format!("Bad id: {e}"))?;
    let store = source_store(state)?;
    let cfg = store
        .get(uuid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Calendar source not found".to_string())?;
    let vault = state
        .vault
        .clone()
        .ok_or_else(|| "Credential vault unavailable".to_string())?;
    crate::calendar_sources::build_source(&cfg, &vault)
        .await
        .map_err(|e| e.to_string())
}
