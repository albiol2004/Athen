//! SQLite-backed `WakeupStore`.
//!
//! One table `wakeups`. Schedule, origin, and the two allowlists are JSON
//! columns — none of these are queried in v1, so a junction table or
//! schedule-specific columns would be over-engineering. The only indexed
//! lookup is the scheduler's "is anything due?" probe via `next_fire_at`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::config::NotificationChannelKind;
use athen_core::contact::ContactId;
use athen_core::error::{AthenError, Result};
use athen_core::traits::wakeup::WakeupStore;
use athen_core::wakeup::{AutonomyBand, Schedule, Wakeup, WakeupOrigin};

const WAKEUPS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS wakeups (
    id TEXT PRIMARY KEY,
    schedule_json TEXT NOT NULL,
    instruction TEXT NOT NULL,
    autonomy TEXT NOT NULL,
    preferred_channel_json TEXT,
    tool_allowlist_json TEXT,
    contact_allowlist_json TEXT,
    profile TEXT NOT NULL,
    arc_id TEXT,
    origin_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_fired_at TEXT,
    next_fire_at TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    inherit_restrictions INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_wakeups_next_fire
    ON wakeups(next_fire_at) WHERE enabled = 1 AND next_fire_at IS NOT NULL;
"#;

/// Idempotent ALTER for existing DBs that pre-date the
/// `inherit_restrictions` column. SQLite returns "duplicate column" if
/// the column is already there — we swallow that exact error and
/// surface anything else.
const WAKEUPS_ADD_INHERIT_SQL: &str =
    "ALTER TABLE wakeups ADD COLUMN inherit_restrictions INTEGER NOT NULL DEFAULT 1";

const WAKEUP_COLS: &str = "id, schedule_json, instruction, autonomy, preferred_channel_json, \
     tool_allowlist_json, contact_allowlist_json, profile, arc_id, \
     origin_json, created_at, last_fired_at, next_fire_at, enabled, inherit_restrictions";

/// SQLite-backed wake-up store.
#[derive(Clone)]
pub struct SqliteWakeupStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteWakeupStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(WAKEUPS_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init wakeups schema: {e}")))?;
            // Migrate older DBs that don't have inherit_restrictions yet.
            // Duplicate-column errors mean the migration already ran.
            if let Err(e) = conn.execute(WAKEUPS_ADD_INHERIT_SQL, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(AthenError::Other(format!(
                        "Migrate wakeups.inherit_restrictions: {e}"
                    )));
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn datetime_to_str(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| AthenError::Other(format!("Parse datetime '{s}': {e}")))
}

fn read_wakeup_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Wakeup> {
    let id_str: String = row.get(0)?;
    let schedule_json: String = row.get(1)?;
    let instruction: String = row.get(2)?;
    let autonomy_str: String = row.get(3)?;
    let preferred_channel_json: Option<String> = row.get(4)?;
    let tool_allowlist_json: Option<String> = row.get(5)?;
    let contact_allowlist_json: Option<String> = row.get(6)?;
    let profile: String = row.get(7)?;
    let arc_id_str: Option<String> = row.get(8)?;
    let origin_json: String = row.get(9)?;
    let created_at_str: String = row.get(10)?;
    let last_fired_at_str: Option<String> = row.get(11)?;
    let next_fire_at_str: Option<String> = row.get(12)?;
    let enabled_int: i64 = row.get(13)?;
    let inherit_restrictions_int: i64 = row.get(14)?;

    let parse_err = |i: usize, e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    };
    let uuid_err = |i: usize, e: uuid::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    };
    let chrono_err = |i: usize, e: chrono::ParseError| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    };

    let id = Uuid::parse_str(&id_str).map_err(|e| uuid_err(0, e))?;
    let schedule: Schedule = serde_json::from_str(&schedule_json).map_err(|e| parse_err(1, e))?;
    let autonomy = AutonomyBand::from_str_lossy(&autonomy_str);
    let preferred_channel: Option<NotificationChannelKind> = match preferred_channel_json {
        Some(j) => Some(serde_json::from_str(&j).map_err(|e| parse_err(4, e))?),
        None => None,
    };
    let tool_allowlist: Option<Vec<String>> = match tool_allowlist_json {
        Some(j) => Some(serde_json::from_str(&j).map_err(|e| parse_err(5, e))?),
        None => None,
    };
    let contact_allowlist: Option<Vec<ContactId>> = match contact_allowlist_json {
        Some(j) => Some(serde_json::from_str(&j).map_err(|e| parse_err(6, e))?),
        None => None,
    };
    let arc_id = arc_id_str;
    let origin: WakeupOrigin = serde_json::from_str(&origin_json).map_err(|e| parse_err(9, e))?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| chrono_err(10, e))?
        .with_timezone(&Utc);
    let last_fired_at = match last_fired_at_str {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(&s)
                .map_err(|e| chrono_err(11, e))?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    let next_fire_at = match next_fire_at_str {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(&s)
                .map_err(|e| chrono_err(12, e))?
                .with_timezone(&Utc),
        ),
        None => None,
    };

    Ok(Wakeup {
        id,
        schedule,
        instruction,
        autonomy,
        preferred_channel,
        tool_allowlist,
        contact_allowlist,
        inherit_restrictions: inherit_restrictions_int != 0,
        profile,
        arc_id,
        origin,
        created_at,
        last_fired_at,
        next_fire_at,
        enabled: enabled_int != 0,
    })
}

#[async_trait]
impl WakeupStore for SqliteWakeupStore {
    async fn create(&self, wakeup: &Wakeup) -> Result<()> {
        let conn = self.conn.clone();
        let w = wakeup.clone();
        // Reject duplicates with a clear message instead of a SQLite UNIQUE
        // constraint error string.
        if self.get(w.id).await?.is_some() {
            return Err(AthenError::Other(format!(
                "Wakeup already exists: {}",
                w.id
            )));
        }
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            insert_or_replace(&conn, &w)?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn update(&self, wakeup: &Wakeup) -> Result<()> {
        let conn = self.conn.clone();
        let w = wakeup.clone();
        if self.get(w.id).await?.is_none() {
            return Err(AthenError::Other(format!("Wakeup not found: {}", w.id)));
        }
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            insert_or_replace(&conn, &w)?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute("DELETE FROM wakeups WHERE id = ?1", params![id.to_string()])
                .map_err(|e| AthenError::Other(format!("Delete wakeup: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Wakeup not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get(&self, id: Uuid) -> Result<Option<Wakeup>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {WAKEUP_COLS} FROM wakeups WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get wakeup: {e}")))?;
            stmt.query_row(params![id.to_string()], read_wakeup_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query wakeup: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_all(&self) -> Result<Vec<Wakeup>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {WAKEUP_COLS} FROM wakeups ORDER BY created_at DESC, id ASC");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_all: {e}")))?;
            let rows = stmt
                .query_map([], read_wakeup_row)
                .map_err(|e| AthenError::Other(format!("Query list_all: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Wakeup row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Wakeup>> {
        let conn = self.conn.clone();
        let cutoff = datetime_to_str(now);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!(
                "SELECT {WAKEUP_COLS} FROM wakeups \
                 WHERE enabled = 1 AND next_fire_at IS NOT NULL \
                 AND datetime(next_fire_at) <= datetime(?1) \
                 ORDER BY next_fire_at ASC, id ASC"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_due: {e}")))?;
            let rows = stmt
                .query_map(params![cutoff], read_wakeup_row)
                .map_err(|e| AthenError::Other(format!("Query list_due: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Wakeup row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn mark_fired(
        &self,
        id: Uuid,
        fired_at: DateTime<Utc>,
        next_fire_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let fired_str = datetime_to_str(fired_at);
        let next_str = next_fire_at.map(datetime_to_str);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "UPDATE wakeups SET last_fired_at = ?1, next_fire_at = ?2 WHERE id = ?3",
                    params![fired_str, next_str, id.to_string()],
                )
                .map_err(|e| AthenError::Other(format!("Mark fired: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Wakeup not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "UPDATE wakeups SET enabled = ?1 WHERE id = ?2",
                    params![enabled as i64, id.to_string()],
                )
                .map_err(|e| AthenError::Other(format!("Set enabled: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Wakeup not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn insert_or_replace(conn: &Connection, w: &Wakeup) -> Result<()> {
    let schedule_json = serde_json::to_string(&w.schedule).map_err(AthenError::Serialization)?;
    let preferred_channel_json = match &w.preferred_channel {
        Some(c) => Some(serde_json::to_string(c).map_err(AthenError::Serialization)?),
        None => None,
    };
    let tool_allowlist_json = match &w.tool_allowlist {
        Some(v) => Some(serde_json::to_string(v).map_err(AthenError::Serialization)?),
        None => None,
    };
    let contact_allowlist_json = match &w.contact_allowlist {
        Some(v) => Some(serde_json::to_string(v).map_err(AthenError::Serialization)?),
        None => None,
    };
    let origin_json = serde_json::to_string(&w.origin).map_err(AthenError::Serialization)?;
    conn.execute(
        "INSERT OR REPLACE INTO wakeups \
         (id, schedule_json, instruction, autonomy, preferred_channel_json, \
          tool_allowlist_json, contact_allowlist_json, profile, arc_id, \
          origin_json, created_at, last_fired_at, next_fire_at, enabled, \
          inherit_restrictions) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            w.id.to_string(),
            schedule_json,
            w.instruction,
            w.autonomy.as_str(),
            preferred_channel_json,
            tool_allowlist_json,
            contact_allowlist_json,
            w.profile,
            w.arc_id.clone(),
            origin_json,
            datetime_to_str(w.created_at),
            w.last_fired_at.map(datetime_to_str),
            w.next_fire_at.map(datetime_to_str),
            w.enabled as i64,
            w.inherit_restrictions as i64,
        ],
    )
    .map_err(|e| AthenError::Other(format!("Insert wakeup: {e}")))?;
    Ok(())
}

// Re-export for tests so callers don't need to depend on chrono parsing.
#[allow(dead_code)]
pub(crate) fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    parse_datetime(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn setup() -> SqliteWakeupStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteWakeupStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    fn mk_wakeup(at: DateTime<Utc>) -> Wakeup {
        Wakeup {
            id: Uuid::new_v4(),
            schedule: Schedule::OneShot { at },
            instruction: "summarize tech news".into(),
            autonomy: AutonomyBand::SafeOnly,
            preferred_channel: Some(NotificationChannelKind::InApp),
            tool_allowlist: None,
            contact_allowlist: None,
            inherit_restrictions: true,
            profile: "assistant".into(),
            arc_id: None,
            origin: WakeupOrigin::User,
            created_at: Utc::now(),
            last_fired_at: None,
            next_fire_at: Some(at),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn create_and_get_round_trips_all_fields() {
        let store = setup().await;
        let arc_id = "arc_20260509_120000".to_string();
        let authoring_arc_id = Uuid::new_v4();
        let contact_id = Uuid::new_v4();
        let now = Utc::now();
        let w = Wakeup {
            id: Uuid::new_v4(),
            schedule: Schedule::Cron {
                expr: "0 8 * * *".into(),
                tz: "Europe/Madrid".into(),
            },
            instruction: "daily news brief".into(),
            autonomy: AutonomyBand::Auto,
            preferred_channel: Some(NotificationChannelKind::Telegram),
            tool_allowlist: Some(vec!["web_search".into(), "read_page".into()]),
            contact_allowlist: Some(vec![contact_id]),
            inherit_restrictions: false,
            profile: "assistant".into(),
            arc_id: Some(arc_id.clone()),
            origin: WakeupOrigin::Agent { authoring_arc_id },
            created_at: now,
            last_fired_at: Some(now - Duration::hours(2)),
            next_fire_at: Some(now + Duration::hours(22)),
            enabled: true,
        };
        store.create(&w).await.unwrap();

        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, w.id);
        assert_eq!(loaded.schedule, w.schedule);
        assert_eq!(loaded.instruction, w.instruction);
        assert_eq!(loaded.autonomy, w.autonomy);
        assert_eq!(loaded.preferred_channel, w.preferred_channel);
        assert_eq!(loaded.tool_allowlist, w.tool_allowlist);
        assert_eq!(loaded.contact_allowlist, w.contact_allowlist);
        assert_eq!(loaded.inherit_restrictions, w.inherit_restrictions);
        assert_eq!(loaded.profile, w.profile);
        assert_eq!(loaded.arc_id, w.arc_id);
        assert_eq!(loaded.origin, w.origin);
        assert_eq!(loaded.enabled, w.enabled);
        // Timestamps may lose sub-second precision via RFC3339; compare seconds.
        assert_eq!(loaded.created_at.timestamp(), w.created_at.timestamp());
        assert_eq!(
            loaded.last_fired_at.unwrap().timestamp(),
            w.last_fired_at.unwrap().timestamp()
        );
        assert_eq!(
            loaded.next_fire_at.unwrap().timestamp(),
            w.next_fire_at.unwrap().timestamp()
        );
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let store = setup().await;
        let w = mk_wakeup(Utc::now() + Duration::hours(1));
        store.create(&w).await.unwrap();
        let err = store.create(&w).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn update_replaces_fields_and_errors_on_missing() {
        let store = setup().await;
        let mut w = mk_wakeup(Utc::now() + Duration::hours(1));
        store.create(&w).await.unwrap();
        w.instruction = "different".into();
        w.autonomy = AutonomyBand::Auto;
        store.update(&w).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert_eq!(loaded.instruction, "different");
        assert_eq!(loaded.autonomy, AutonomyBand::Auto);

        let ghost = mk_wakeup(Utc::now());
        let err = store.update(&ghost).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn delete_removes_and_errors_on_missing() {
        let store = setup().await;
        let w = mk_wakeup(Utc::now() + Duration::hours(1));
        store.create(&w).await.unwrap();
        store.delete(w.id).await.unwrap();
        assert!(store.get(w.id).await.unwrap().is_none());

        let err = store.delete(Uuid::new_v4()).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn list_all_orders_by_created_at_desc() {
        let store = setup().await;
        let mut a = mk_wakeup(Utc::now() + Duration::hours(1));
        let mut b = mk_wakeup(Utc::now() + Duration::hours(2));
        a.created_at = Utc::now() - Duration::hours(1);
        b.created_at = Utc::now();
        store.create(&a).await.unwrap();
        store.create(&b).await.unwrap();
        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, b.id);
        assert_eq!(all[1].id, a.id);
    }

    #[tokio::test]
    async fn list_due_returns_only_enabled_with_next_fire_at_in_past() {
        let store = setup().await;
        let now = Utc::now();
        let due_past = mk_wakeup(now - Duration::minutes(10));
        let due_now = mk_wakeup(now);
        let future = mk_wakeup(now + Duration::hours(1));
        let mut disabled = mk_wakeup(now - Duration::minutes(5));
        disabled.enabled = false;
        let mut no_next = mk_wakeup(now);
        no_next.next_fire_at = None;

        store.create(&due_past).await.unwrap();
        store.create(&due_now).await.unwrap();
        store.create(&future).await.unwrap();
        store.create(&disabled).await.unwrap();
        store.create(&no_next).await.unwrap();

        let due = store.list_due(now).await.unwrap();
        let due_ids: Vec<Uuid> = due.iter().map(|w| w.id).collect();
        assert!(due_ids.contains(&due_past.id));
        assert!(due_ids.contains(&due_now.id));
        assert!(!due_ids.contains(&future.id));
        assert!(!due_ids.contains(&disabled.id));
        assert!(!due_ids.contains(&no_next.id));
        // earliest first
        assert_eq!(due[0].id, due_past.id);
    }

    #[tokio::test]
    async fn mark_fired_updates_last_and_next() {
        let store = setup().await;
        let w = mk_wakeup(Utc::now() - Duration::minutes(1));
        store.create(&w).await.unwrap();
        let fired_at = Utc::now();
        let next = fired_at + Duration::hours(24);
        store.mark_fired(w.id, fired_at, Some(next)).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.last_fired_at.unwrap().timestamp(),
            fired_at.timestamp()
        );
        assert_eq!(loaded.next_fire_at.unwrap().timestamp(), next.timestamp());
    }

    #[tokio::test]
    async fn mark_fired_with_none_next_clears_schedule() {
        // One-shot wake-up after firing has no next fire — the row stays for
        // history but list_due skips it.
        let store = setup().await;
        let w = mk_wakeup(Utc::now() - Duration::minutes(1));
        store.create(&w).await.unwrap();
        store.mark_fired(w.id, Utc::now(), None).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert!(loaded.next_fire_at.is_none());
        let due = store
            .list_due(Utc::now() + Duration::days(365))
            .await
            .unwrap();
        assert!(due.iter().all(|x| x.id != w.id));
    }

    #[tokio::test]
    async fn mark_fired_errors_on_missing() {
        let store = setup().await;
        let err = store
            .mark_fired(Uuid::new_v4(), Utc::now(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn set_enabled_toggles_and_affects_list_due() {
        let store = setup().await;
        let w = mk_wakeup(Utc::now() - Duration::minutes(1));
        store.create(&w).await.unwrap();
        // initially due
        let due = store.list_due(Utc::now()).await.unwrap();
        assert!(due.iter().any(|x| x.id == w.id));

        store.set_enabled(w.id, false).await.unwrap();
        let due = store.list_due(Utc::now()).await.unwrap();
        assert!(due.iter().all(|x| x.id != w.id));
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert!(!loaded.enabled);

        // re-enable
        store.set_enabled(w.id, true).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert!(loaded.enabled);
    }

    #[tokio::test]
    async fn set_enabled_errors_on_missing() {
        let store = setup().await;
        let err = store.set_enabled(Uuid::new_v4(), false).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn agent_origin_round_trips() {
        let store = setup().await;
        let arc_id = Uuid::new_v4();
        let mut w = mk_wakeup(Utc::now());
        w.origin = WakeupOrigin::Agent {
            authoring_arc_id: arc_id,
        };
        store.create(&w).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        match loaded.origin {
            WakeupOrigin::Agent { authoring_arc_id } => assert_eq!(authoring_arc_id, arc_id),
            _ => panic!("expected agent origin"),
        }
    }

    #[tokio::test]
    async fn interval_schedule_round_trips() {
        let store = setup().await;
        let anchor = Utc::now();
        let mut w = mk_wakeup(anchor + Duration::hours(1));
        w.schedule = Schedule::Interval {
            every_seconds: 7200,
            anchor,
        };
        store.create(&w).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        match loaded.schedule {
            Schedule::Interval {
                every_seconds,
                anchor: a,
            } => {
                assert_eq!(every_seconds, 7200);
                assert_eq!(a.timestamp(), anchor.timestamp());
            }
            _ => panic!("expected interval"),
        }
    }

    #[tokio::test]
    async fn allowlists_default_to_none_and_round_trip_when_set() {
        let store = setup().await;
        let mut w = mk_wakeup(Utc::now());
        store.create(&w).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert!(loaded.tool_allowlist.is_none());
        assert!(loaded.contact_allowlist.is_none());

        w.tool_allowlist = Some(vec!["a".into(), "b".into()]);
        w.contact_allowlist = Some(vec![Uuid::new_v4(), Uuid::new_v4()]);
        store.update(&w).await.unwrap();
        let loaded = store.get(w.id).await.unwrap().unwrap();
        assert_eq!(loaded.tool_allowlist.as_ref().unwrap().len(), 2);
        assert_eq!(loaded.contact_allowlist.as_ref().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn parse_dt_helper_round_trips() {
        let now = Utc::now();
        let s = datetime_to_str(now);
        let back = parse_dt(&s).unwrap();
        assert_eq!(back.timestamp(), now.timestamp());
    }
}
