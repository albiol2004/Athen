//! Calendar source factory + read-side sync loop.
//!
//! The factory turns a persisted [`CalendarSourceConfig`] into a live
//! `Box<dyn CalendarSource>` by pulling the password from the vault and
//! routing on `kind`. The sync loop spawns one tokio task per enabled
//! source that polls remote events on the configured interval and
//! reconciles them into the local [`CalendarStore`].
//!
//! Write direction (agent edits a local event → push to remote) is
//! intentionally deferred — for v1 the agent's calendar tools operate
//! on the local store only, and the next remote pull observes the
//! discrepancy. A future task wires real push-through.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use url::Url;
use uuid::Uuid;

use athen_caldav::CalDavSource;
use athen_core::calendar_source_config::{CalendarSourceConfig, CalendarSourceKind};
use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source::{CalendarSource, RemoteEvent};
use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;
use athen_core::traits::vault::Vault;
use athen_persistence::calendar::{CalendarEvent, CalendarStore, EventCreator};

/// How far ahead each pull pulls. One year covers everything the
/// calendar UI is likely to surface — holidays, annual reminders,
/// future planning — without the user having to babysit window size.
const PULL_WINDOW_DAYS_AHEAD: i64 = 365;

/// How far back each pull pulls. One year of history matches what a
/// real calendar app exposes when the user pages backwards. The earlier
/// 1-day window was sized for "reminders only" semantics and made every
/// calendar look empty as soon as the user scrolled past today.
const PULL_WINDOW_DAYS_BEHIND: i64 = 365;

/// Build a `Box<dyn CalendarSource>` for a configured source. Reads the
/// password out of the vault using the config's stored scope/key.
pub async fn build_source(
    config: &CalendarSourceConfig,
    vault: &Arc<dyn Vault>,
) -> Result<Box<dyn CalendarSource>> {
    let password = vault
        .get(&config.vault_scope, &config.vault_key)
        .await?
        .ok_or_else(|| {
            AthenError::Other(format!(
                "Calendar source {} has no password in vault at scope `{}`",
                config.display_name, config.vault_scope
            ))
        })?;

    match config.kind {
        CalendarSourceKind::Caldav => {
            let url = Url::parse(&config.base_url).map_err(|e| {
                AthenError::Other(format!(
                    "Invalid base_url `{}` for source `{}`: {e}",
                    config.base_url, config.display_name
                ))
            })?;
            let source = CalDavSource::new(
                config.id.to_string(),
                config.display_name.clone(),
                url,
                &config.username,
                &password,
            )?;
            Ok(Box::new(source))
        }
    }
}

/// Spawn one background sync task per enabled source. Each task runs on
/// `config.sync_interval_secs`. The caller-supplied `shutdown_tx`'s
/// receivers are used for cancellation — sending one `()` on it
/// terminates every sync loop at its next select boundary.
///
/// When `app_handle` is `Some`, each completed pass that changed anything
/// emits a `calendar-sync-completed` Tauri event so the open Calendar UI
/// can reload events without waiting for the user to navigate.
pub fn spawn_sync_loops(
    sources: Vec<CalendarSourceConfig>,
    vault: Arc<dyn Vault>,
    calendar_store: CalendarStore,
    config_store: Arc<dyn CalendarSourceConfigStore>,
    shutdown_tx: broadcast::Sender<()>,
    app_handle: Option<tauri::AppHandle>,
) {
    for cfg in sources {
        if !cfg.enabled {
            continue;
        }
        let vault = vault.clone();
        let store = calendar_store.clone();
        let cfg_store = config_store.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let interval = Duration::from_secs(cfg.sync_interval_secs.max(60));
        let source_name = cfg.display_name.clone();
        let id = cfg.id;
        let app_handle = app_handle.clone();
        tokio::spawn(async move {
            tracing::info!(source = %source_name, interval_secs = interval.as_secs(), "Calendar sync loop started");
            // First pass on a short delay so startup logs don't pile up
            // and so we don't hit the remote at the exact same instant
            // every cold start.
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                match run_one_sync_pass(&cfg, &vault, &store, &cfg_store).await {
                    Ok(stats) => {
                        tracing::info!(
                            source = %source_name,
                            inserted = stats.inserted,
                            updated = stats.updated,
                            deleted = stats.deleted,
                            "Calendar sync pass complete"
                        );
                        if let Some(handle) = app_handle.as_ref() {
                            emit_sync_completed(handle, id, &source_name, stats);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(source = %source_name, error = %e, "Calendar sync pass failed");
                        // Best-effort error stamping — if this fails too
                        // we've already logged the original error.
                        let _ = cfg_store.record_sync_error(id, &e.to_string()).await;
                    }
                }
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        tracing::info!(source = %source_name, "Calendar sync loop shutdown signal");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });
    }
}

/// Emit a `calendar-sync-completed` event to the frontend. No-op if it
/// can't serialise (it can't fail in practice for this payload).
fn emit_sync_completed(
    handle: &tauri::AppHandle,
    source_id: Uuid,
    source_name: &str,
    stats: SyncStats,
) {
    use tauri::Emitter as _;
    let payload = serde_json::json!({
        "source_id": source_id.to_string(),
        "source_name": source_name,
        "inserted": stats.inserted,
        "updated": stats.updated,
        "deleted": stats.deleted,
    });
    if let Err(e) = handle.emit("calendar-sync-completed", payload) {
        tracing::debug!(error = %e, "Failed to emit calendar-sync-completed");
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Public façade over `run_one_sync_pass` for the Settings panel's
/// "Sync now" button. Same body, exposed name.
pub async fn sync_one(
    config: &CalendarSourceConfig,
    vault: &Arc<dyn Vault>,
    calendar_store: &CalendarStore,
    config_store: &Arc<dyn CalendarSourceConfigStore>,
) -> Result<SyncStats> {
    run_one_sync_pass(config, vault, calendar_store, config_store).await
}

/// What the write-through path uses to address one calendar on one source.
#[derive(Debug, Clone)]
pub struct WriteTarget {
    pub source: CalendarSourceConfig,
    pub calendar_id: String,
    pub calendar_name: String,
}

/// Auto-pick a write target. Returns `Some` only when exactly one source
/// is enabled and we can identify one calendar to write into.
///
/// Picking rule:
/// 1. Filter to enabled sources. If zero or more than one, return `None` —
///    the user must disambiguate, and we don't have a UI for that yet.
/// 2. If `selected_calendars` is non-empty, use the first entry.
/// 3. Else fetch `list_calendars()` and skip calendars whose name contains
///    a reminders keyword (we PUT VEVENTs, not VTODOs). First survivor wins.
pub async fn auto_pick_write_target(
    config_store: &Arc<dyn CalendarSourceConfigStore>,
    vault: &Arc<dyn Vault>,
) -> Result<Option<WriteTarget>> {
    let sources = config_store.list().await?;
    let enabled: Vec<CalendarSourceConfig> = sources.into_iter().filter(|s| s.enabled).collect();
    if enabled.len() != 1 {
        return Ok(None);
    }
    let source_cfg = enabled.into_iter().next().unwrap();
    let live = build_source(&source_cfg, vault).await?;

    let (cal_id, cal_name) = if let Some(first) = source_cfg.selected_calendars.first() {
        // We only have the id — look up the name from list_calendars for
        // the toast. Best-effort; fall back to a slice of the id.
        let name = match live.list_calendars().await {
            Ok(cals) => cals
                .into_iter()
                .find(|c| &c.id == first)
                .map(|c| c.name)
                .unwrap_or_else(|| first.clone()),
            Err(_) => first.clone(),
        };
        (first.clone(), name)
    } else {
        let cals = live.list_calendars().await?;
        let pick = cals
            .into_iter()
            .find(|c| !is_reminders_name(&c.name))
            .ok_or_else(|| {
                AthenError::Other(
                    "No writable calendar found on source — every collection looks like a reminders/VTODO calendar".into(),
                )
            })?;
        (pick.id, pick.name)
    };

    Ok(Some(WriteTarget {
        source: source_cfg,
        calendar_id: cal_id,
        calendar_name: cal_name,
    }))
}

fn is_reminders_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("recordator") || lower.contains("reminder") || lower.contains("to-do")
}

/// Push a freshly-created local event to the remote, then return the
/// remote_id + etag for the caller to stamp onto the local row.
pub async fn push_create(
    target: &WriteTarget,
    vault: &Arc<dyn Vault>,
    event: &CalendarEvent,
) -> Result<(String, Option<String>, String)> {
    let live = build_source(&target.source, vault).await?;
    let mut remote = local_to_remote(event, &target.calendar_id)?;
    // Ensure a UID exists; CalDAV needs it for the .ics path.
    let uid = remote
        .ical_uid
        .clone()
        .unwrap_or_else(|| format!("athen-{}@local", event.id));
    remote.ical_uid = Some(uid.clone());
    let (remote_id, etag) = live.create_event(&target.calendar_id, &remote).await?;
    Ok((remote_id, etag, uid))
}

/// Push an update to the remote when the local row was previously synced.
pub async fn push_update(
    source_cfg: &CalendarSourceConfig,
    vault: &Arc<dyn Vault>,
    event: &CalendarEvent,
) -> Result<Option<String>> {
    let live = build_source(source_cfg, vault).await?;
    // The CalDAV adapter ignores `calendar_id` on update (the remote_id is
    // the full object URL already). Pass empty to keep the trait happy.
    let calendar_id = "";
    let remote = local_to_remote(event, calendar_id)?;
    let remote_id = event.remote_id.as_deref().ok_or_else(|| {
        AthenError::Other("Cannot push update: local event has no remote_id".into())
    })?;
    let new_etag = live
        .update_event(
            calendar_id,
            remote_id,
            event.remote_etag.as_deref(),
            &remote,
        )
        .await?;
    Ok(new_etag)
}

/// Delete the remote object for a synced local row.
pub async fn push_delete(
    source_cfg: &CalendarSourceConfig,
    vault: &Arc<dyn Vault>,
    event: &CalendarEvent,
) -> Result<()> {
    let live = build_source(source_cfg, vault).await?;
    let remote_id = event.remote_id.as_deref().ok_or_else(|| {
        AthenError::Other("Cannot push delete: local event has no remote_id".into())
    })?;
    live.delete_event("", remote_id, event.remote_etag.as_deref())
        .await
}

fn local_to_remote(event: &CalendarEvent, calendar_id: &str) -> Result<RemoteEvent> {
    let start = chrono::DateTime::parse_from_rfc3339(&event.start_time)
        .map_err(|e| AthenError::Other(format!("Bad start_time `{}`: {e}", event.start_time)))?
        .with_timezone(&Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&event.end_time)
        .map_err(|e| AthenError::Other(format!("Bad end_time `{}`: {e}", event.end_time)))?
        .with_timezone(&Utc);
    Ok(RemoteEvent {
        remote_id: event.remote_id.clone().unwrap_or_default(),
        calendar_id: calendar_id.to_string(),
        etag: event.remote_etag.clone(),
        ical_uid: event.ical_uid.clone(),
        title: event.title.clone(),
        description: event.description.clone(),
        start_time: start,
        end_time: end,
        all_day: event.all_day,
        location: event.location.clone(),
        recurrence_rrule: None,
        reminder_minutes: event.reminder_minutes.clone(),
    })
}

/// Run one read-side reconciliation pass against the remote.
///
/// Records the result on the config row before returning so the
/// Settings UI can show "last sync 3 minutes ago" / "auth failed at …".
async fn run_one_sync_pass(
    config: &CalendarSourceConfig,
    vault: &Arc<dyn Vault>,
    calendar_store: &CalendarStore,
    config_store: &Arc<dyn CalendarSourceConfigStore>,
) -> Result<SyncStats> {
    let source = build_source(config, vault).await?;
    let now = Utc::now();
    let pull_start = now - chrono::Duration::days(PULL_WINDOW_DAYS_BEHIND);
    let pull_end = now + chrono::Duration::days(PULL_WINDOW_DAYS_AHEAD);

    // Decide which calendars to sync: the user-selected list, or all
    // exposed by the source when none were picked.
    let calendars = if config.selected_calendars.is_empty() {
        let discovered = source.list_calendars().await?;
        tracing::info!(
            source = %config.display_name,
            discovered = discovered.len(),
            names = ?discovered.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "Calendar sync: list_calendars returned"
        );
        if discovered.is_empty() {
            tracing::warn!(
                source = %config.display_name,
                base_url = %config.base_url,
                username = %config.username,
                "Calendar sync: no calendars discovered — check credentials and home-set URL"
            );
        }
        discovered.into_iter().map(|c| c.id).collect::<Vec<_>>()
    } else {
        config.selected_calendars.clone()
    };

    let mut stats = SyncStats::default();
    let source_id = config.id.to_string();
    // Aggregate every remote_id we see across every calendar this pass
    // pulls. We must NOT run reconcile_deletes per calendar — `source_id`
    // covers the whole source, so a per-calendar delete would treat
    // events from sibling calendars as "missing from the pull" and nuke
    // them every pass. Build the union first, then delete once at the end.
    let mut all_pulled_remote_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for calendar_id in &calendars {
        let pulled = source
            .list_events(calendar_id, pull_start, pull_end)
            .await?;
        tracing::info!(
            source = %config.display_name,
            calendar_id = %calendar_id,
            pulled = pulled.len(),
            "Calendar sync: list_events returned"
        );

        // INSERT / UPDATE every pulled event.
        for remote in &pulled {
            all_pulled_remote_ids.insert(remote.remote_id.clone());
            let result = reconcile_one(calendar_store, &source_id, remote).await?;
            match result {
                ReconcileResult::Inserted => stats.inserted += 1,
                ReconcileResult::Updated => stats.updated += 1,
                ReconcileResult::Unchanged => {}
            }
        }
    }

    // DELETE: anything in the local DB tagged with this source_id
    // and whose start falls inside the pull window but didn't appear
    // in *any* calendar's pull — the user removed it on the remote side.
    let deleted = reconcile_deletes(
        calendar_store,
        &source_id,
        &all_pulled_remote_ids,
        pull_start,
        pull_end,
    )
    .await?;
    stats.deleted += deleted;

    config_store.record_sync_success(config.id, now).await?;
    Ok(stats)
}

#[derive(Debug, Clone, Copy)]
enum ReconcileResult {
    Inserted,
    Updated,
    Unchanged,
}

/// Insert, update, or no-op a single pulled event. Match by
/// `(source_id, remote_id)`. ETag mismatch → update; same etag → no-op.
async fn reconcile_one(
    store: &CalendarStore,
    source_id: &str,
    remote: &RemoteEvent,
) -> Result<ReconcileResult> {
    let local = find_by_remote_id(store, source_id, &remote.remote_id).await?;
    let now = Utc::now().to_rfc3339();
    let mapped = remote_to_local_event(remote, source_id, local.as_ref(), &now);

    match local {
        None => {
            store.create_event(&mapped).await?;
            Ok(ReconcileResult::Inserted)
        }
        Some(existing) => {
            if existing.remote_etag == mapped.remote_etag && etag_present(&existing.remote_etag) {
                return Ok(ReconcileResult::Unchanged);
            }
            store.update_event(&mapped).await?;
            Ok(ReconcileResult::Updated)
        }
    }
}

fn etag_present(etag: &Option<String>) -> bool {
    etag.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
}

/// Linear scan over the source's rows to find a matching remote_id.
/// Cheap in practice — sources typically have ≤ a few hundred events
/// in the pull window — and we avoid adding a dedicated query method
/// just for the sync loop. If this ever shows up in a profile we add
/// `CalendarStore::find_by_remote_id`.
async fn find_by_remote_id(
    store: &CalendarStore,
    source_id: &str,
    remote_id: &str,
) -> Result<Option<CalendarEvent>> {
    let all = store.list_all_events().await?;
    Ok(all.into_iter().find(|e| {
        e.source_id.as_deref() == Some(source_id) && e.remote_id.as_deref() == Some(remote_id)
    }))
}

fn remote_to_local_event(
    remote: &RemoteEvent,
    source_id: &str,
    existing: Option<&CalendarEvent>,
    now_rfc3339: &str,
) -> CalendarEvent {
    // Preserve the local UUID id on update so any other code that
    // referenced it (links from arcs, agent notes) stays valid.
    let id = existing
        .map(|e| e.id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let created_at = existing
        .map(|e| e.created_at.clone())
        .unwrap_or_else(|| now_rfc3339.to_string());
    CalendarEvent {
        id,
        title: remote.title.clone(),
        description: remote.description.clone(),
        start_time: remote.start_time.to_rfc3339(),
        end_time: remote.end_time.to_rfc3339(),
        all_day: remote.all_day,
        location: remote.location.clone(),
        recurrence: None, // Athen's local `Recurrence` enum is coarser than RRULE; v1 leaves it null.
        reminder_minutes: remote.reminder_minutes.clone(),
        color: None,
        category: existing.and_then(|e| e.category.clone()),
        created_by: existing
            .map(|e| e.created_by.clone())
            .unwrap_or(EventCreator::User),
        arc_id: existing.and_then(|e| e.arc_id.clone()),
        created_at,
        updated_at: now_rfc3339.to_string(),
        source_id: Some(source_id.to_string()),
        remote_id: Some(remote.remote_id.clone()),
        remote_etag: remote.etag.clone(),
        ical_uid: remote.ical_uid.clone(),
    }
}

/// Find rows tagged with this source whose start falls in the pulled
/// window but were not returned in the latest pull, and delete them.
async fn reconcile_deletes(
    store: &CalendarStore,
    source_id: &str,
    pulled_remote_ids: &std::collections::HashSet<String>,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Result<usize> {
    let all = store.list_all_events().await?;
    let mut deleted = 0usize;
    for ev in all {
        if ev.source_id.as_deref() != Some(source_id) {
            continue;
        }
        let Some(remote_id) = ev.remote_id.as_deref() else {
            continue;
        };
        if pulled_remote_ids.contains(remote_id) {
            continue;
        }
        // Only delete if the event's start falls within the pull window.
        // Events outside the window aren't refreshed by this pull and
        // their absence does not imply remote deletion.
        let start = match DateTime::parse_from_rfc3339(&ev.start_time) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(_) => continue,
        };
        if start < window_start || start > window_end {
            continue;
        }
        store.delete_event(&ev.id).await?;
        deleted += 1;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex;

    fn remote_event(remote_id: &str, title: &str, etag: &str, start: DateTime<Utc>) -> RemoteEvent {
        RemoteEvent {
            remote_id: remote_id.into(),
            calendar_id: "cal".into(),
            etag: Some(etag.into()),
            ical_uid: Some(format!("{remote_id}-uid")),
            title: title.into(),
            description: None,
            start_time: start,
            end_time: start + chrono::Duration::hours(1),
            all_day: false,
            location: None,
            recurrence_rrule: None,
            reminder_minutes: vec![],
        }
    }

    async fn fresh_store() -> CalendarStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let conn = StdArc::new(Mutex::new(conn));
        let store = CalendarStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn reconcile_one_inserts_then_updates() {
        let store = fresh_store().await;
        let now = Utc::now();
        let r1 = remote_event("evt-1", "Lunch", "\"v1\"", now + chrono::Duration::hours(2));

        // First reconcile → insert.
        let res = reconcile_one(&store, "src-1", &r1).await.unwrap();
        assert!(matches!(res, ReconcileResult::Inserted));
        assert_eq!(store.list_all_events().await.unwrap().len(), 1);

        // Same etag → unchanged.
        let res = reconcile_one(&store, "src-1", &r1).await.unwrap();
        assert!(matches!(res, ReconcileResult::Unchanged));

        // Different etag → update, same row.
        let r2 = remote_event(
            "evt-1",
            "Lunch (moved)",
            "\"v2\"",
            now + chrono::Duration::hours(3),
        );
        let res = reconcile_one(&store, "src-1", &r2).await.unwrap();
        assert!(matches!(res, ReconcileResult::Updated));
        let all = store.list_all_events().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Lunch (moved)");
    }

    #[tokio::test]
    async fn reconcile_deletes_only_within_window() {
        let store = fresh_store().await;
        let now = Utc::now();

        // Two events in DB from the same source, one inside the next-30d window, one 60 days out.
        let near = remote_event("near", "Near", "\"e\"", now + chrono::Duration::days(5));
        let far = remote_event("far", "Far", "\"e\"", now + chrono::Duration::days(60));
        reconcile_one(&store, "src-1", &near).await.unwrap();
        reconcile_one(&store, "src-1", &far).await.unwrap();
        assert_eq!(store.list_all_events().await.unwrap().len(), 2);

        // Now simulate a pull that returned nothing.
        let pulled: std::collections::HashSet<String> = std::collections::HashSet::new();
        let win_start = now - chrono::Duration::days(1);
        let win_end = now + chrono::Duration::days(30);
        let deleted = reconcile_deletes(&store, "src-1", &pulled, win_start, win_end)
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        // The far event survived.
        let all = store.list_all_events().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Far");
    }

    #[tokio::test]
    async fn reconcile_deletes_never_touches_local_only_events() {
        let store = fresh_store().await;
        let now = Utc::now();

        // A local-only event (source_id = None).
        let local = CalendarEvent {
            id: "local-1".into(),
            title: "Personal note".into(),
            description: None,
            start_time: (now + chrono::Duration::hours(2)).to_rfc3339(),
            end_time: (now + chrono::Duration::hours(3)).to_rfc3339(),
            all_day: false,
            location: None,
            recurrence: None,
            reminder_minutes: vec![],
            color: None,
            category: None,
            created_by: EventCreator::User,
            arc_id: None,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            source_id: None,
            remote_id: None,
            remote_etag: None,
            ical_uid: None,
        };
        store.create_event(&local).await.unwrap();

        let pulled: std::collections::HashSet<String> = std::collections::HashSet::new();
        let win_start = now - chrono::Duration::days(1);
        let win_end = now + chrono::Duration::days(30);
        let deleted = reconcile_deletes(&store, "src-1", &pulled, win_start, win_end)
            .await
            .unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.list_all_events().await.unwrap().len(), 1);
    }
}
