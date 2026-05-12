//! Persistent state for enabled MCP servers.
//!
//! Tracks which catalog entries the user has enabled and the per-entry
//! configuration (a JSON blob whose schema is declared by the catalog).

use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::traits::mcp::McpCatalogEntry;

const MCP_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS mcp_enabled (
    mcp_id TEXT PRIMARY KEY,
    config TEXT NOT NULL DEFAULT '{}',
    enabled_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS mcp_custom_entries (
    id TEXT PRIMARY KEY,
    definition TEXT NOT NULL,
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

    /// Persist a user-supplied custom catalog entry (BYO MCP).
    ///
    /// The full `McpCatalogEntry` is serialized into the `definition`
    /// column so it can be re-hydrated at startup without a catalog
    /// lookup. The companion `mcp_enabled` row (with the per-instance
    /// config blob) is still required to actually run the server — call
    /// `enable(id, config)` after `add_custom`.
    pub async fn add_custom(&self, entry: &McpCatalogEntry) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        let def = serde_json::to_string(entry)
            .map_err(|e| AthenError::Other(format!("serialize custom mcp: {e}")))?;
        conn.execute(
            "INSERT INTO mcp_custom_entries (id, definition, enabled_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET definition = excluded.definition",
            params![entry.id, def, now],
        )
        .map_err(|e| AthenError::Other(format!("insert mcp_custom_entries: {e}")))?;
        Ok(())
    }

    /// Remove a custom catalog entry. Does NOT touch `mcp_enabled` — the
    /// caller should also call `disable(id)` to drop the running instance.
    pub async fn remove_custom(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM mcp_custom_entries WHERE id = ?1", params![id])
            .map_err(|e| AthenError::Other(format!("delete mcp_custom_entries: {e}")))?;
        Ok(())
    }

    /// List every user-supplied custom catalog entry, regardless of
    /// whether it is currently enabled.
    pub async fn list_custom(&self) -> Result<Vec<McpCatalogEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT definition FROM mcp_custom_entries ORDER BY enabled_at ASC")
            .map_err(|e| AthenError::Other(format!("prepare list_custom: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let def: String = row.get(0)?;
                Ok(def)
            })
            .map_err(|e| AthenError::Other(format!("query list_custom: {e}")))?;

        let mut out = Vec::new();
        for r in rows {
            let def = r.map_err(|e| AthenError::Other(format!("row: {e}")))?;
            match serde_json::from_str::<McpCatalogEntry>(&def) {
                Ok(entry) => out.push(entry),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping corrupt custom MCP definition");
                }
            }
        }
        Ok(out)
    }

    /// Fetch a single custom catalog entry by id.
    pub async fn get_custom(&self, id: &str) -> Result<Option<McpCatalogEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT definition FROM mcp_custom_entries WHERE id = ?1")
            .map_err(|e| AthenError::Other(format!("prepare get_custom: {e}")))?;
        let mut rows = stmt
            .query(params![id])
            .map_err(|e| AthenError::Other(format!("query get_custom: {e}")))?;
        match rows
            .next()
            .map_err(|e| AthenError::Other(format!("row: {e}")))?
        {
            Some(row) => {
                let def: String = row
                    .get(0)
                    .map_err(|e| AthenError::Other(format!("col 0: {e}")))?;
                let entry = serde_json::from_str::<McpCatalogEntry>(&def)
                    .map_err(|e| AthenError::Other(format!("parse custom mcp: {e}")))?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
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
            .enable("slack", &serde_json::json!({"workspace": "athen"}))
            .await
            .unwrap();
        let listed = store.list_enabled().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].mcp_id, "slack");
        assert_eq!(listed[0].config["workspace"], "athen");
    }

    #[tokio::test]
    async fn enable_overwrites_config() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        store
            .enable("slack", &serde_json::json!({"workspace": "first"}))
            .await
            .unwrap();
        store
            .enable("slack", &serde_json::json!({"workspace": "second"}))
            .await
            .unwrap();
        let listed = store.list_enabled().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].config["workspace"], "second");
    }

    #[tokio::test]
    async fn disable_removes() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        store.enable("slack", &serde_json::json!({})).await.unwrap();
        store.disable("slack").await.unwrap();
        assert!(store.list_enabled().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_returns_none_when_disabled() {
        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        assert!(store.get("slack").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_entry_roundtrip() {
        use athen_core::risk::BaseImpact;
        use athen_core::traits::mcp::{EnvBinding, EnvValue, McpCatalogEntry, McpSource};

        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        let entry = McpCatalogEntry {
            id: "byo-github".into(),
            display_name: "GitHub (BYO)".into(),
            description: "User-installed GitHub MCP".into(),
            icon: None,
            config_schema: serde_json::json!({}),
            source: McpSource::Process {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                env: vec![EnvBinding {
                    key: "GITHUB_TOKEN".into(),
                    value: EnvValue::Vault {
                        scope: "mcp:byo-github".into(),
                        key: "token".into(),
                    },
                }],
                working_dir: None,
            },
            base_risk: BaseImpact::WritePersist,
        };

        store.add_custom(&entry).await.unwrap();
        let listed = store.list_custom().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "byo-github");
        assert_eq!(listed[0].display_name, "GitHub (BYO)");

        let fetched = store.get_custom("byo-github").await.unwrap().unwrap();
        match fetched.source {
            McpSource::Process { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 2);
            }
            _ => panic!("expected Process"),
        }

        store.remove_custom("byo-github").await.unwrap();
        assert!(store.list_custom().await.unwrap().is_empty());
        assert!(store.get_custom("byo-github").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_and_enabled_are_independent_tables() {
        use athen_core::risk::BaseImpact;
        use athen_core::traits::mcp::{McpCatalogEntry, McpSource};

        let db = Database::in_memory().await.unwrap();
        let store = db.mcp_store();
        let entry = McpCatalogEntry {
            id: "byo".into(),
            display_name: "BYO".into(),
            description: String::new(),
            icon: None,
            config_schema: serde_json::json!({}),
            source: McpSource::Process {
                command: "/bin/true".into(),
                args: vec![],
                env: vec![],
                working_dir: None,
            },
            base_risk: BaseImpact::Read,
        };
        store.add_custom(&entry).await.unwrap();
        // No `enable()` call — the custom definition exists but no live instance.
        assert!(store.list_enabled().await.unwrap().is_empty());
        assert_eq!(store.list_custom().await.unwrap().len(), 1);
    }
}
