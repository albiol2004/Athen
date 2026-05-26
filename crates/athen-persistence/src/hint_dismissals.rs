//! Persistence for proactive hint dismissals.
//!
//! One table: `hint_dismissals`. Tracks which hints the user dismissed
//! and whether the dismissal is permanent ("don't show again").

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintDismissal {
    pub hint_id: String,
    pub permanent: bool,
    pub dismissed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct HintDismissalStore {
    conn: Arc<Mutex<Connection>>,
}

impl HintDismissalStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS hint_dismissals (
                hint_id     TEXT PRIMARY KEY,
                permanent   INTEGER NOT NULL DEFAULT 0,
                dismissed_at TEXT NOT NULL
            );",
        )
        .map_err(|e| AthenError::Other(format!("Init hint_dismissals schema: {e}")))?;
        Ok(())
    }

    pub async fn dismiss(&self, hint_id: &str, permanent: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO hint_dismissals (hint_id, permanent, dismissed_at)
             VALUES (?1, ?2, ?3)",
            params![hint_id, permanent as i32, now],
        )
        .map_err(|e| AthenError::Other(format!("Dismiss hint: {e}")))?;
        Ok(())
    }

    pub async fn is_dismissed(&self, hint_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let permanent: bool = conn
            .query_row(
                "SELECT permanent FROM hint_dismissals WHERE hint_id = ?1",
                params![hint_id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        Ok(permanent)
    }

    pub async fn list_permanent(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT hint_id FROM hint_dismissals WHERE permanent = 1")
            .map_err(|e| AthenError::Other(format!("List permanent dismissals: {e}")))?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| AthenError::Other(format!("Query permanent dismissals: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    pub async fn undismiss(&self, hint_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM hint_dismissals WHERE hint_id = ?1",
            params![hint_id],
        )
        .map_err(|e| AthenError::Other(format!("Undismiss hint: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Database;

    #[tokio::test]
    async fn dismiss_and_check() {
        let db = Database::in_memory().await.unwrap();
        let store = db.hint_dismissal_store();

        assert!(!store.is_dismissed("no_email").await.unwrap());

        store.dismiss("no_email", false).await.unwrap();
        assert!(!store.is_dismissed("no_email").await.unwrap());

        store.dismiss("no_email", true).await.unwrap();
        assert!(store.is_dismissed("no_email").await.unwrap());
    }

    #[tokio::test]
    async fn list_permanent() {
        let db = Database::in_memory().await.unwrap();
        let store = db.hint_dismissal_store();

        store.dismiss("a", true).await.unwrap();
        store.dismiss("b", false).await.unwrap();
        store.dismiss("c", true).await.unwrap();

        let perm = store.list_permanent().await.unwrap();
        assert_eq!(perm.len(), 2);
        assert!(perm.contains(&"a".to_string()));
        assert!(perm.contains(&"c".to_string()));
    }

    #[tokio::test]
    async fn undismiss() {
        let db = Database::in_memory().await.unwrap();
        let store = db.hint_dismissal_store();

        store.dismiss("x", true).await.unwrap();
        assert!(store.is_dismissed("x").await.unwrap());

        store.undismiss("x").await.unwrap();
        assert!(!store.is_dismissed("x").await.unwrap());
    }
}
