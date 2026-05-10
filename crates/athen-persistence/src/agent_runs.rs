//! SQLite-backed `agent_runs` historical record.
//!
//! One table: `agent_runs`. Tracks the lifecycle of every agent execution
//! (start → step bumps → finalize) so the UI's "watch the agents work"
//! panel can show recent + ongoing runs even after a restart. Live state
//! (current tool, last action) lives in `crates/athen-app/src/agent_registry.rs`;
//! this store is the durable companion.
//!
//! Pruned to a 30-day window by `AppState::start_agent_run_pruner`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS agent_runs (
    task_id TEXT PRIMARY KEY,
    arc_id TEXT,
    source TEXT NOT NULL,
    title TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL,
    step_count INTEGER NOT NULL DEFAULT 0,
    profile_id TEXT,
    model TEXT,
    error TEXT
);

CREATE INDEX IF NOT EXISTS idx_agent_runs_started ON agent_runs(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_agent_runs_arc ON agent_runs(arc_id);
"#;

const COLS: &str = "task_id, arc_id, source, title, started_at, finished_at, \
                    status, step_count, profile_id, model, error";

/// Durable record of an agent run. `started_at` / `finished_at` are
/// stored as RFC3339 strings, matching the rest of the persistence layer
/// (`wakeups`, `http_endpoints`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunRecord {
    pub task_id: String,
    pub arc_id: Option<String>,
    pub source: String,
    pub title: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub step_count: u32,
    pub profile_id: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct SqliteAgentRunStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteAgentRunStore {
    /// Construct from a shared connection handle. Mirrors
    /// `SqliteWakeupStore::new`. Use `Database::agent_run_store()` to
    /// build one against the app's primary connection.
    pub fn from_conn(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init agent_runs schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Insert (or replace) the row for an agent run with `status="running"`.
    /// REPLACE makes a re-register with the same task_id idempotent.
    pub async fn start(&self, run: &AgentRunRecord) -> Result<()> {
        let conn = self.conn.clone();
        let r = run.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR REPLACE INTO agent_runs \
                 (task_id, arc_id, source, title, started_at, finished_at, \
                  status, step_count, profile_id, model, error) \
                 VALUES (?1,?2,?3,?4,?5,NULL,'running',?6,?7,?8,NULL)",
                params![
                    r.task_id,
                    r.arc_id,
                    r.source,
                    r.title,
                    datetime_to_str(r.started_at),
                    r.step_count as i64,
                    r.profile_id,
                    r.model,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Insert agent_run: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    pub async fn bump_step(&self, task_id: &str, step_count: u32) -> Result<()> {
        let conn = self.conn.clone();
        let id = task_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE agent_runs SET step_count = ?1 WHERE task_id = ?2",
                params![step_count as i64, id],
            )
            .map_err(|e| AthenError::Other(format!("Bump step: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    pub async fn finalize(
        &self,
        task_id: &str,
        status: &str,
        error: Option<&str>,
        finished_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let id = task_id.to_string();
        let status = status.to_string();
        let err = error.map(|s| s.to_string());
        let fin = datetime_to_str(finished_at);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE agent_runs SET status = ?1, finished_at = ?2, error = ?3 \
                 WHERE task_id = ?4",
                params![status, fin, err, id],
            )
            .map_err(|e| AthenError::Other(format!("Finalize agent_run: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    pub async fn list_recent(&self, limit: u32) -> Result<Vec<AgentRunRecord>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {COLS} FROM agent_runs ORDER BY started_at DESC LIMIT ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_recent: {e}")))?;
            let rows = stmt
                .query_map(params![limit as i64], read_row)
                .map_err(|e| AthenError::Other(format!("Query list_recent: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("agent_run row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    pub async fn get(&self, task_id: &str) -> Result<Option<AgentRunRecord>> {
        let conn = self.conn.clone();
        let id = task_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {COLS} FROM agent_runs WHERE task_id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get: {e}")))?;
            stmt.query_row(params![id], read_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query agent_run: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Delete every finalized row whose `finished_at` is strictly older
    /// than `cutoff`. Running rows (where `finished_at IS NULL`) are
    /// always kept — they may be active across an Athen restart.
    /// Returns the number of rows deleted.
    pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let conn = self.conn.clone();
        let cutoff_s = datetime_to_str(cutoff);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM agent_runs \
                     WHERE finished_at IS NOT NULL \
                     AND datetime(finished_at) < datetime(?1)",
                    params![cutoff_s],
                )
                .map_err(|e| AthenError::Other(format!("Prune agent_runs: {e}")))?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn datetime_to_str(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_datetime(s: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRunRecord> {
    let task_id: String = row.get(0)?;
    let arc_id: Option<String> = row.get(1)?;
    let source: String = row.get(2)?;
    let title: String = row.get(3)?;
    let started_at_str: String = row.get(4)?;
    let finished_at_str: Option<String> = row.get(5)?;
    let status: String = row.get(6)?;
    let step_count_i: i64 = row.get(7)?;
    let profile_id: Option<String> = row.get(8)?;
    let model: Option<String> = row.get(9)?;
    let error: Option<String> = row.get(10)?;

    let chrono_err = |i: usize, e: chrono::ParseError| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    };

    let started_at = parse_datetime(&started_at_str).map_err(|e| chrono_err(4, e))?;
    let finished_at = match finished_at_str {
        Some(s) => Some(parse_datetime(&s).map_err(|e| chrono_err(5, e))?),
        None => None,
    };

    Ok(AgentRunRecord {
        task_id,
        arc_id,
        source,
        title,
        started_at,
        finished_at,
        status,
        step_count: step_count_i.max(0) as u32,
        profile_id,
        model,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn setup() -> SqliteAgentRunStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteAgentRunStore::from_conn(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    fn mk_run(id: &str) -> AgentRunRecord {
        AgentRunRecord {
            task_id: id.to_string(),
            arc_id: Some("arc_20260510_120000".to_string()),
            source: "user_chat".to_string(),
            title: "Help me draft an email".to_string(),
            started_at: Utc::now(),
            finished_at: None,
            status: "running".to_string(),
            step_count: 0,
            profile_id: Some("default".to_string()),
            model: Some("deepseek-chat".to_string()),
            error: None,
        }
    }

    #[tokio::test]
    async fn start_then_bump_then_finalize_then_list() {
        let store = setup().await;
        let run = mk_run("11111111-1111-1111-1111-111111111111");
        store.start(&run).await.unwrap();

        // Mid-run bumps
        store.bump_step(&run.task_id, 1).await.unwrap();
        store.bump_step(&run.task_id, 5).await.unwrap();

        // Finalize
        let fin = Utc::now();
        store
            .finalize(&run.task_id, "completed", None, fin)
            .await
            .unwrap();

        let listed = store.list_recent(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        let row = &listed[0];
        assert_eq!(row.task_id, run.task_id);
        assert_eq!(row.status, "completed");
        assert_eq!(row.step_count, 5);
        assert_eq!(row.title, run.title);
        assert!(row.finished_at.is_some());
        assert!(row.error.is_none());
    }

    #[tokio::test]
    async fn start_is_idempotent_via_replace() {
        let store = setup().await;
        let run = mk_run("22222222-2222-2222-2222-222222222222");
        store.start(&run).await.unwrap();
        // Re-start with same task_id should not error and should reset to running.
        store.start(&run).await.unwrap();
        let got = store.get(&run.task_id).await.unwrap().unwrap();
        assert_eq!(got.status, "running");
    }

    #[tokio::test]
    async fn finalize_records_error_message() {
        let store = setup().await;
        let run = mk_run("33333333-3333-3333-3333-333333333333");
        store.start(&run).await.unwrap();
        store
            .finalize(&run.task_id, "failed", Some("boom"), Utc::now())
            .await
            .unwrap();
        let got = store.get(&run.task_id).await.unwrap().unwrap();
        assert_eq!(got.status, "failed");
        assert_eq!(got.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn list_recent_orders_started_desc_and_respects_limit() {
        let store = setup().await;
        let mut a = mk_run("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let mut b = mk_run("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        a.started_at = Utc::now() - Duration::minutes(10);
        b.started_at = Utc::now();
        store.start(&a).await.unwrap();
        store.start(&b).await.unwrap();

        let listed = store.list_recent(10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].task_id, b.task_id);
        assert_eq!(listed[1].task_id, a.task_id);

        let limited = store.list_recent(1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].task_id, b.task_id);
    }

    #[tokio::test]
    async fn prune_drops_old_finished_keeps_running_and_recent() {
        let store = setup().await;
        let now = Utc::now();
        let cutoff = now - Duration::days(30);

        let mut old = mk_run("00000000-0000-0000-0000-000000000001");
        old.started_at = now - Duration::days(40);
        store.start(&old).await.unwrap();
        store
            .finalize(&old.task_id, "completed", None, now - Duration::days(35))
            .await
            .unwrap();

        let mut recent = mk_run("00000000-0000-0000-0000-000000000002");
        recent.started_at = now - Duration::days(2);
        store.start(&recent).await.unwrap();
        store
            .finalize(&recent.task_id, "completed", None, now - Duration::days(1))
            .await
            .unwrap();

        let running = mk_run("00000000-0000-0000-0000-000000000003");
        store.start(&running).await.unwrap();

        let n = store.prune_older_than(cutoff).await.unwrap();
        assert_eq!(n, 1);

        let all = store.list_recent(10).await.unwrap();
        let ids: Vec<String> = all.iter().map(|r| r.task_id.clone()).collect();
        assert!(!ids.contains(&old.task_id));
        assert!(ids.contains(&recent.task_id));
        assert!(ids.contains(&running.task_id));
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let store = setup().await;
        assert!(store.get("does-not-exist").await.unwrap().is_none());
    }
}
