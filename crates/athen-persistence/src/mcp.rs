//! Persistent state for enabled MCP servers.
//!
//! Tracks which catalog entries the user has enabled and the per-entry
//! configuration (a JSON blob whose schema is declared by the catalog).

use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

const MCP_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS mcp_enabled (
    mcp_id TEXT PRIMARY KEY,
    config TEXT NOT NULL DEFAULT '{}',
    enabled_at TEXT NOT NULL
);
";

/// One enabled MCP entry with its user-supplied configuration.
#[derive(Debug, Clone)]
pub struct EnabledMcp {
    pub mcp_id: String,
    pub config: serde_json::Value,
}

/// SQLite-backed store for enabled MCP state.
#[derive(Clone)]
pub struct McpStore {
    conn: Arc<Mutex<Connection>>,
}

impl McpStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute_batch(MCP_SCHEMA_SQL)
            .map_err(|e| AthenError::Other(format!("init mcp schema: {e}")))
    }

    pub async fn list_enabled(&self) -> Result<Vec<EnabledMcp>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT mcp_id, config FROM mcp_enabled ORDER BY enabled_at ASC")
            .map_err(|e| AthenError::Other(format!("prepare list_enabled: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let cfg: String = row.get(1)?;
                Ok((id, cfg))
            })
            .map_err(|e| AthenError::Other(format!("query list_enabled: {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let (id, cfg_str) = r.map_err(|e| AthenError::Other(format!("row: {e}")))?;
            let config = serde_json::from_str(&cfg_str).unwrap_or(serde_json::json!({}));
            out.push(EnabledMcp { mcp_id: id, config });
        }
        Ok(out)
    }

    pub async fn enable(&self, mcp_id: &str, config: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        let cfg_str = serde_json::to_string(config)
            .map_err(|e| AthenError::Other(format!("serialize config: {e}")))?;
        conn.execute(
            "INSERT INTO mcp_enabled (mcp_id, config, enabled_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(mcp_id) DO UPDATE SET config = excluded.config",
            params![mcp_id, cfg_str, now],
        )
        .map_err(|e| AthenError::Other(format!("insert mcp_enabled: {e}")))?;
        Ok(())
    }

    pub async fn disable(&self, mcp_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM mcp_enabled WHERE mcp_id = ?1", params![mcp_id])
            .map_err(|e| AthenError::Other(format!("delete mcp_enabled: {e}")))?;
        Ok(())
    }

    pub async fn get(&self, mcp_id: &str) -> Result<Option<EnabledMcp>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT mcp_id, config FROM mcp_enabled WHERE mcp_id = ?1")
            .map_err(|e| AthenError::Other(format!("prepare get: {e}")))?;
        let mut rows = stmt
            .query(params![mcp_id])
            .map_err(|e| AthenError::Other(format!("query get: {e}")))?;
        if let Some(row) = rows
            .next()
            .map_err(|e| AthenError::Other(format!("row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| AthenError::Other(format!("col 0: {e}")))?;
            let cfg_str: String = row
                .get(1)
                .map_err(|e| AthenError::Other(format!("col 1: {e}")))?;
            let config = serde_json::from_str(&cfg_str).unwrap_or(serde_json::json!({}));
            Ok(Some(EnabledMcp { mcp_id: id, config }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Database;

    #[tokio::test]
    async fn enable_and_list() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        store
            .enable("files", &serde_json::json!({"sandbox_root": "/tmp"}))
            .await
            .unwrap();
        let listed = store.list_enabled().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].mcp_id, "files");
        assert_eq!(listed[0].config["sandbox_root"], "/tmp");
    }

    #[tokio::test]
    async fn enable_overwrites_config() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        store
            .enable("files", &serde_json::json!({"sandbox_root": "/tmp"}))
            .await
            .unwrap();
        store
            .enable("files", &serde_json::json!({"sandbox_root": "/home"}))
            .await
            .unwrap();
        let listed = store.list_enabled().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].config["sandbox_root"], "/home");
    }

    #[tokio::test]
    async fn disable_removes() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        store.enable("files", &serde_json::json!({})).await.unwrap();
        store.disable("files").await.unwrap();
        assert!(store.list_enabled().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_returns_none_when_disabled() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        assert!(store.get("files").await.unwrap().is_none());
    }
}
