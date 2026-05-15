//! SQLite-backed `CalendarSourceConfigStore`.
//!
//! One table: `calendar_sources`. `selected_calendars` is a JSON array;
//! sources are loaded holistically by the sync loop so a relational
//! sub-table for selected calendars would be over-engineered.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::calendar_source_config::{CalendarSourceConfig, CalendarSourceKind};
use athen_core::error::{AthenError, Result};
use athen_core::traits::calendar_source_config::CalendarSourceConfigStore;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS calendar_sources (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    display_name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    username TEXT NOT NULL,
    vault_scope TEXT NOT NULL,
    vault_key TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    selected_calendars_json TEXT NOT NULL DEFAULT '[]',
    sync_interval_secs INTEGER NOT NULL DEFAULT 300,
    last_sync_at TEXT,
    last_sync_error TEXT,
    created_at TEXT NOT NULL
);
"#;

const COLS: &str = "id, kind, display_name, base_url, username, vault_scope, vault_key, \
enabled, selected_calendars_json, sync_interval_secs, last_sync_at, last_sync_error, created_at";

pub struct SqliteCalendarSourceStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCalendarSourceStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init calendar_sources schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn row_to_config(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarSourceConfig> {
    let id_str: String = row.get(0)?;
    let kind_str: String = row.get(1)?;
    let display_name: String = row.get(2)?;
    let base_url: String = row.get(3)?;
    let username: String = row.get(4)?;
    let vault_scope: String = row.get(5)?;
    let vault_key: String = row.get(6)?;
    let enabled: i64 = row.get(7)?;
    let selected_json: String = row.get(8)?;
    let sync_interval_secs: i64 = row.get(9)?;
    let last_sync_at_str: Option<String> = row.get(10)?;
    let last_sync_error: Option<String> = row.get(11)?;
    let created_at_str: String = row.get(12)?;

    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind = CalendarSourceKind::from_str(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unknown calendar source kind `{kind_str}`"),
            )),
        )
    })?;
    let selected_calendars: Vec<String> = serde_json::from_str(&selected_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let last_sync_at: Option<DateTime<Utc>> = last_sync_at_str
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);

    Ok(CalendarSourceConfig {
        id,
        kind,
        display_name,
        base_url,
        username,
        vault_scope,
        vault_key,
        enabled: enabled != 0,
        selected_calendars,
        sync_interval_secs: sync_interval_secs.max(0) as u64,
        last_sync_at,
        last_sync_error,
        created_at,
    })
}

#[async_trait]
impl CalendarSourceConfigStore for SqliteCalendarSourceStore {
    async fn list(&self) -> Result<Vec<CalendarSourceConfig>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLS} FROM calendar_sources ORDER BY created_at ASC"
                ))
                .map_err(|e| AthenError::Other(format!("Prepare list calendar_sources: {e}")))?;
            let rows = stmt
                .query_map([], row_to_config)
                .map_err(|e| AthenError::Other(format!("Query list calendar_sources: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Read source row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get(&self, id: Uuid) -> Result<Option<CalendarSourceConfig>> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLS} FROM calendar_sources WHERE id = ?1"
                ))
                .map_err(|e| AthenError::Other(format!("Prepare get source: {e}")))?;
            let mut rows = stmt
                .query_map(params![id_s], row_to_config)
                .map_err(|e| AthenError::Other(format!("Query get source: {e}")))?;
            match rows.next() {
                Some(Ok(c)) => Ok(Some(c)),
                Some(Err(e)) => Err(AthenError::Other(format!("Read source row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn upsert(&self, config: &CalendarSourceConfig) -> Result<()> {
        let conn = self.conn.clone();
        let cfg = config.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let selected_json = serde_json::to_string(&cfg.selected_calendars)
                .map_err(|e| AthenError::Other(format!("Serialize selected calendars: {e}")))?;
            // Preserve created_at on replace by reading the existing row first.
            let existing_created: Option<String> = conn
                .query_row(
                    "SELECT created_at FROM calendar_sources WHERE id = ?1",
                    params![cfg.id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .ok();
            let created_at = existing_created.unwrap_or_else(|| cfg.created_at.to_rfc3339());
            conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO calendar_sources ({COLS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
                ),
                params![
                    cfg.id.to_string(),
                    cfg.kind.as_str(),
                    cfg.display_name,
                    cfg.base_url,
                    cfg.username,
                    cfg.vault_scope,
                    cfg.vault_key,
                    cfg.enabled as i32,
                    selected_json,
                    cfg.sync_interval_secs as i64,
                    cfg.last_sync_at.map(|d| d.to_rfc3339()),
                    cfg.last_sync_error,
                    created_at,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Upsert calendar source: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM calendar_sources WHERE id = ?1", params![id_s])
                .map_err(|e| AthenError::Other(format!("Delete calendar source: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE calendar_sources SET enabled = ?1 WHERE id = ?2",
                params![enabled as i32, id_s],
            )
            .map_err(|e| AthenError::Other(format!("Set enabled: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn set_selected_calendars(&self, id: Uuid, calendars: &[String]) -> Result<()> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        let json = serde_json::to_string(calendars)
            .map_err(|e| AthenError::Other(format!("Serialize selected calendars: {e}")))?;
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE calendar_sources SET selected_calendars_json = ?1 WHERE id = ?2",
                params![json, id_s],
            )
            .map_err(|e| AthenError::Other(format!("Set selected calendars: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn record_sync_success(&self, id: Uuid, at: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        let at_s = at.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE calendar_sources SET last_sync_at = ?1, last_sync_error = NULL WHERE id = ?2",
                params![at_s, id_s],
            )
            .map_err(|e| AthenError::Other(format!("Record sync success: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn record_sync_error(&self, id: Uuid, error: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        let err_s = error.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE calendar_sources SET last_sync_error = ?1 WHERE id = ?2",
                params![err_s, id_s],
            )
            .map_err(|e| AthenError::Other(format!("Record sync error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqliteCalendarSourceStore {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = SqliteCalendarSourceStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn upsert_and_list() {
        let store = setup().await;
        let cfg = CalendarSourceConfig::new_caldav(
            "iCloud (me@me.com)",
            "https://caldav.icloud.com/",
            "me@me.com",
        );
        store.upsert(&cfg).await.unwrap();
        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, cfg.id);
        assert_eq!(all[0].kind, CalendarSourceKind::Caldav);
        assert_eq!(all[0].display_name, "iCloud (me@me.com)");
        assert!(all[0].enabled);
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let store = setup().await;
        let cfg = CalendarSourceConfig::new_caldav("x", "https://x", "x");
        store.upsert(&cfg).await.unwrap();
        store.delete(cfg.id).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn record_sync_clears_error() {
        let store = setup().await;
        let cfg = CalendarSourceConfig::new_caldav("x", "https://x", "x");
        store.upsert(&cfg).await.unwrap();
        store.record_sync_error(cfg.id, "boom").await.unwrap();
        let row = store.get(cfg.id).await.unwrap().unwrap();
        assert_eq!(row.last_sync_error.as_deref(), Some("boom"));
        store.record_sync_success(cfg.id, Utc::now()).await.unwrap();
        let row = store.get(cfg.id).await.unwrap().unwrap();
        assert!(row.last_sync_error.is_none());
        assert!(row.last_sync_at.is_some());
    }

    #[tokio::test]
    async fn upsert_preserves_created_at_on_replace() {
        let store = setup().await;
        let mut cfg = CalendarSourceConfig::new_caldav("x", "https://x", "x");
        let original_created = cfg.created_at;
        store.upsert(&cfg).await.unwrap();

        cfg.display_name = "x renamed".into();
        cfg.created_at = Utc::now() + chrono::Duration::days(1); // would-be tamper
        store.upsert(&cfg).await.unwrap();

        let row = store.get(cfg.id).await.unwrap().unwrap();
        // Within a second of the original — second-resolution RFC3339 round-trip.
        let drift = (row.created_at - original_created).num_seconds().abs();
        assert!(drift <= 1, "created_at drifted by {drift}s");
    }

    #[tokio::test]
    async fn set_selected_calendars_round_trip() {
        let store = setup().await;
        let cfg = CalendarSourceConfig::new_caldav("x", "https://x", "x");
        store.upsert(&cfg).await.unwrap();
        let ids = vec!["cal-a".to_string(), "cal-b".to_string()];
        store.set_selected_calendars(cfg.id, &ids).await.unwrap();
        let row = store.get(cfg.id).await.unwrap().unwrap();
        assert_eq!(row.selected_calendars, ids);
    }
}
