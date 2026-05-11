//! Per-`chat_id` Telegram transcript store.
//!
//! Records every inbound and outbound Telegram message Athen sees so the
//! owner-Telegram handler can prepend a few recent turns as system
//! context, independent of which arc the new message gets routed to.
//! This is the safety net for the cross-channel routing problem:
//! even when arc-pick lands on the wrong arc (or creates a fresh one),
//! the agent still has continuity from this chat's recent exchange.
//!
//! Bounded retention by row count per `chat_id` (default cap below)
//! so the table can't grow unboundedly on a busy chat.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

/// How many of the most-recent rows we keep per `chat_id`. The fetch
/// path asks for ≤ 20, so a 200-row cap is ample buffer while keeping
/// the table small enough that the per-write prune is cheap.
const PER_CHAT_RETENTION: usize = 200;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS telegram_chat_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id INTEGER NOT NULL,
    direction TEXT NOT NULL,
    text TEXT NOT NULL,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    ts TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_telegram_chat_log_chat_ts
    ON telegram_chat_log(chat_id, ts);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramLogDirection {
    Inbound,
    Outbound,
}

impl TelegramLogDirection {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Inbound => "in",
            Self::Outbound => "out",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelegramLogEntry {
    pub chat_id: i64,
    pub direction: TelegramLogDirection,
    pub text: String,
    pub has_attachments: bool,
    /// RFC3339 UTC timestamp.
    pub ts: String,
}

pub struct TelegramChatLogStore {
    conn: Arc<Mutex<Connection>>,
}

impl TelegramChatLogStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init telegram_chat_log schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Append one entry and prune to the per-`chat_id` retention cap.
    pub async fn append(
        &self,
        chat_id: i64,
        direction: TelegramLogDirection,
        text: &str,
        has_attachments: bool,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let dir = direction.as_str().to_string();
        let text = text.to_string();
        let ts = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO telegram_chat_log (chat_id, direction, text, has_attachments, ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chat_id, dir, text, has_attachments as i64, ts],
            )
            .map_err(|e| AthenError::Other(format!("Insert telegram_chat_log: {e}")))?;
            // Prune: keep only the most-recent N rows for this chat.
            conn.execute(
                "DELETE FROM telegram_chat_log \
                 WHERE chat_id = ?1 \
                   AND id NOT IN ( \
                     SELECT id FROM telegram_chat_log \
                     WHERE chat_id = ?1 \
                     ORDER BY id DESC \
                     LIMIT ?2 \
                   )",
                params![chat_id, PER_CHAT_RETENTION as i64],
            )
            .map_err(|e| AthenError::Other(format!("Prune telegram_chat_log: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Fetch the most-recent `limit` rows for `chat_id`, ordered oldest →
    /// newest so the caller can format them as a transcript directly.
    pub async fn recent(&self, chat_id: i64, limit: usize) -> Result<Vec<TelegramLogEntry>> {
        let conn = self.conn.clone();
        // Hard cap matches the per-chat retention cap. Callers normally
        // ask for ≤ 20 — the cap exists only so a buggy caller can't
        // ask for "everything" and pull back the entire transcript.
        let limit = limit.min(PER_CHAT_RETENTION);
        tokio::task::spawn_blocking(move || -> Result<Vec<TelegramLogEntry>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT chat_id, direction, text, has_attachments, ts \
                     FROM telegram_chat_log \
                     WHERE chat_id = ?1 \
                     ORDER BY id DESC \
                     LIMIT ?2",
                )
                .map_err(|e| AthenError::Other(format!("Prepare recent: {e}")))?;
            let rows = stmt
                .query_map(params![chat_id, limit as i64], |row| {
                    let dir: String = row.get(1)?;
                    let direction = match dir.as_str() {
                        "in" => TelegramLogDirection::Inbound,
                        _ => TelegramLogDirection::Outbound,
                    };
                    Ok(TelegramLogEntry {
                        chat_id: row.get(0)?,
                        direction,
                        text: row.get(2)?,
                        has_attachments: row.get::<_, i64>(3)? != 0,
                        ts: row.get(4)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query recent: {e}")))?;
            let mut out: Vec<TelegramLogEntry> = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Row recent: {e}")))?);
            }
            // We queried DESC for the LIMIT; reverse so callers get
            // oldest → newest (chronological transcript order).
            out.reverse();
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    async fn store() -> TelegramChatLogStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = TelegramChatLogStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn append_and_recent_returns_in_chronological_order() {
        let s = store().await;
        s.append(42, TelegramLogDirection::Inbound, "hi", false)
            .await
            .unwrap();
        s.append(42, TelegramLogDirection::Outbound, "hey", false)
            .await
            .unwrap();
        s.append(42, TelegramLogDirection::Inbound, "what's up", true)
            .await
            .unwrap();
        let rows = s.recent(42, 10).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "hi");
        assert_eq!(rows[0].direction, TelegramLogDirection::Inbound);
        assert_eq!(rows[1].text, "hey");
        assert_eq!(rows[1].direction, TelegramLogDirection::Outbound);
        assert_eq!(rows[2].text, "what's up");
        assert!(rows[2].has_attachments);
    }

    #[tokio::test]
    async fn recent_is_scoped_to_chat_id() {
        let s = store().await;
        s.append(1, TelegramLogDirection::Inbound, "chat one", false)
            .await
            .unwrap();
        s.append(2, TelegramLogDirection::Inbound, "chat two", false)
            .await
            .unwrap();
        let rows = s.recent(1, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "chat one");
    }

    #[tokio::test]
    async fn retention_caps_per_chat() {
        let s = store().await;
        for i in 0..(PER_CHAT_RETENTION + 10) {
            s.append(7, TelegramLogDirection::Inbound, &format!("m{i}"), false)
                .await
                .unwrap();
        }
        let rows = s.recent(7, PER_CHAT_RETENTION + 50).await.unwrap();
        assert_eq!(rows.len(), PER_CHAT_RETENTION);
        // Oldest surviving row is m10 (we wrote PER_CHAT_RETENTION+10 rows).
        assert_eq!(rows[0].text, "m10");
        assert_eq!(
            rows.last().unwrap().text,
            format!("m{}", PER_CHAT_RETENTION + 9)
        );
    }

    #[tokio::test]
    async fn recent_caps_limit_at_retention() {
        let s = store().await;
        for i in 0..(PER_CHAT_RETENTION + 50) {
            s.append(3, TelegramLogDirection::Inbound, &format!("m{i}"), false)
                .await
                .unwrap();
        }
        // Asking for far more than retention returns at most the
        // retention cap.
        let rows = s.recent(3, 100_000).await.unwrap();
        assert_eq!(rows.len(), PER_CHAT_RETENTION);
    }
}
