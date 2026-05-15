//! SQLite persistence for Athen.
//!
//! Tasks, checkpoints, pending messages, chat history, arcs, and operational state.

pub mod agent_runs;
pub mod arcs;
pub mod attachments;
pub mod calendar;
pub mod calendar_sources;
pub mod chat;
pub mod checkpoint;
pub mod contacts;
pub mod grants;
pub mod http_endpoints;
pub mod identity;
pub mod mcp;
pub mod notifications;
pub mod profiles;
pub mod skills;
pub mod store;
pub mod telegram_chat_log;
pub mod wakeups;

use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

use crate::agent_runs::SqliteAgentRunStore;
use crate::arcs::ArcStore;
use crate::attachments::AttachmentStore;
use crate::calendar::CalendarStore;
use crate::calendar_sources::SqliteCalendarSourceStore;
use crate::chat::ChatStore;
use crate::contacts::SqliteContactStore;
use crate::grants::GrantStore;
use crate::http_endpoints::SqliteHttpEndpointStore;
use crate::identity::SqliteIdentityStore;
use crate::mcp::McpStore;
use crate::notifications::NotificationStore;
use crate::profiles::SqliteProfileStore;
use crate::skills::SqliteSkillStore;
use crate::store::SqliteStore;
use crate::wakeups::SqliteWakeupStore;

/// Owns the SQLite connection and provides access to the store.
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open (or create) a database at the given file path and run migrations.
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || {
            Connection::open(&path).map_err(|e| AthenError::Other(format!("Open database: {e}")))
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))??;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations().await?;
        Ok(db)
    }

    /// Create an in-memory database for testing.
    pub async fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| AthenError::Other(format!("Open in-memory database: {e}")))?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations().await?;
        Ok(db)
    }

    /// Run schema migrations via the store's init_schema method.
    async fn run_migrations(&self) -> Result<()> {
        let store = self.store();
        store.init_schema().await?;
        let chat = self.chat_store();
        chat.init_schema().await?;
        let arcs = self.arc_store();
        arcs.init_schema().await?;
        let calendar = self.calendar_store();
        calendar.init_schema().await?;
        let calendar_sources = self.calendar_source_store();
        calendar_sources.init_schema().await?;
        let contacts = self.contact_store();
        contacts.init_schema().await?;
        let notifications = self.notification_store();
        notifications.init_schema().await?;
        let mcp = self.mcp_store();
        mcp.init_schema().await?;
        let grants = self.grant_store();
        grants.init_schema().await?;
        let profiles = self.profile_store();
        profiles.init_schema().await?;
        let attachments = self.attachment_store();
        attachments.init_schema().await?;
        let identity = self.identity_store();
        identity.init_schema().await?;
        identity.seed_categories_if_empty().await?;
        // Skills: only the index schema lives in the DB; the filesystem
        // store is constructed at the composition root with a skills_dir.
        crate::skills::init_schema(&self.conn).await?;
        let wakeups = self.wakeup_store();
        wakeups.init_schema().await?;
        let endpoints = self.http_endpoint_store();
        endpoints.init_schema().await?;
        let agent_runs = self.agent_run_store();
        agent_runs.init_schema().await?;
        let tg_log = self.telegram_chat_log_store();
        tg_log.init_schema().await?;
        profiles.seed_builtins_if_empty().await
    }

    /// Create a `SqliteStore` backed by this database's connection.
    pub fn store(&self) -> SqliteStore {
        SqliteStore::new(self.conn.clone())
    }

    /// Create a `ChatStore` backed by this database's connection.
    pub fn chat_store(&self) -> ChatStore {
        ChatStore::new(self.conn.clone())
    }

    /// Create an `ArcStore` backed by this database's connection.
    pub fn arc_store(&self) -> ArcStore {
        ArcStore::new(self.conn.clone())
    }

    /// Create a `CalendarStore` backed by this database's connection.
    pub fn calendar_store(&self) -> CalendarStore {
        CalendarStore::new(self.conn.clone())
    }

    /// Create a `SqliteCalendarSourceStore` backed by this database's connection.
    pub fn calendar_source_store(&self) -> SqliteCalendarSourceStore {
        SqliteCalendarSourceStore::new(self.conn.clone())
    }

    /// Create a `SqliteContactStore` backed by this database's connection.
    pub fn contact_store(&self) -> SqliteContactStore {
        SqliteContactStore::new(self.conn.clone())
    }

    /// Create a `NotificationStore` backed by this database's connection.
    pub fn notification_store(&self) -> NotificationStore {
        NotificationStore::new(self.conn.clone())
    }

    /// Create a `McpStore` backed by this database's connection.
    pub fn mcp_store(&self) -> McpStore {
        McpStore::new(self.conn.clone())
    }

    /// Create a `GrantStore` backed by this database's connection.
    pub fn grant_store(&self) -> GrantStore {
        GrantStore::new(self.conn.clone())
    }

    /// Create a `SqliteProfileStore` backed by this database's connection.
    pub fn profile_store(&self) -> SqliteProfileStore {
        SqliteProfileStore::new(self.conn.clone())
    }

    /// Create an `AttachmentStore` backed by this database's connection.
    pub fn attachment_store(&self) -> AttachmentStore {
        AttachmentStore::new(self.conn.clone())
    }

    /// Create a `SqliteIdentityStore` backed by this database's connection.
    pub fn identity_store(&self) -> SqliteIdentityStore {
        SqliteIdentityStore::new(self.conn.clone())
    }

    /// Create a `SqliteSkillStore` backed by this database's connection and
    /// the given filesystem root for `SKILL.md` files. Callers should run
    /// `SkillStore::sync` once after construction so the index reflects any
    /// hand-edits or fresh git-clones into the directory.
    pub fn skill_store(&self, skills_dir: std::path::PathBuf) -> SqliteSkillStore {
        SqliteSkillStore::new(self.conn.clone(), skills_dir)
    }

    /// Create a `SqliteWakeupStore` backed by this database's connection.
    pub fn wakeup_store(&self) -> SqliteWakeupStore {
        SqliteWakeupStore::new(self.conn.clone())
    }

    /// Create a `SqliteHttpEndpointStore` backed by this database's connection.
    pub fn http_endpoint_store(&self) -> SqliteHttpEndpointStore {
        SqliteHttpEndpointStore::new(self.conn.clone())
    }

    /// Create a `SqliteAgentRunStore` backed by this database's connection.
    pub fn agent_run_store(&self) -> SqliteAgentRunStore {
        SqliteAgentRunStore::from_conn(self.conn.clone())
    }

    /// Create a `TelegramChatLogStore` backed by this database's
    /// connection. Records per-`chat_id` Telegram transcripts for the
    /// owner-Telegram handler to prepend as system context.
    pub fn telegram_chat_log_store(&self) -> crate::telegram_chat_log::TelegramChatLogStore {
        crate::telegram_chat_log::TelegramChatLogStore::new(self.conn.clone())
    }

    /// Force-checkpoint the WAL and truncate it. Called from the graceful
    /// shutdown coordinator so an abrupt power loss right after exit
    /// doesn't lose committed-but-WAL'd writes. Best-effort: the pragma
    /// can fail (busy, IO error) and we just log without propagating.
    ///
    /// Uses `tokio::task::spawn_blocking` because the rusqlite call is
    /// synchronous and we don't want to stall the async reactor under the
    /// `tokio::sync::Mutex`.
    pub async fn checkpoint_wal(&self) {
        let conn = self.conn.clone();
        let res = tokio::task::spawn_blocking(move || {
            let guard = conn.blocking_lock();
            guard.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        })
        .await;
        match res {
            Ok(Ok(())) => {
                tracing::debug!("WAL checkpoint completed");
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "WAL checkpoint failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "WAL checkpoint task panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
    use athen_core::traits::persistence::PersistentStore;
    use chrono::Utc;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_database_in_memory() {
        let db = Database::in_memory().await.unwrap();
        let store = db.store();

        let task = Task {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_event: None,
            domain: DomainType::Code,
            description: "Integration test".to_string(),
            priority: TaskPriority::High,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: vec![],
            deadline: None,
        };

        store.save_task(&task).await.unwrap();
        let loaded = store.load_task(task.id).await.unwrap();
        assert!(loaded.is_some());
    }

    #[tokio::test]
    async fn test_database_file_based() {
        let tmp = std::env::temp_dir().join(format!("athen_db_test_{}.db", Uuid::new_v4()));
        let db = Database::new(&tmp).await.unwrap();
        let store = db.store();

        let task = Task {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_event: None,
            domain: DomainType::Research,
            description: "File-based test".to_string(),
            priority: TaskPriority::Low,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: vec![],
            deadline: None,
        };

        store.save_task(&task).await.unwrap();
        let loaded = store.load_task(task.id).await.unwrap();
        assert!(loaded.is_some());

        // Cleanup
        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }
}
