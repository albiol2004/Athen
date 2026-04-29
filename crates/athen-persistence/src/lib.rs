//! SQLite persistence for Athen.
//!
//! Tasks, checkpoints, pending messages, chat history, arcs, and operational state.

pub mod arcs;
pub mod calendar;
pub mod chat;
pub mod checkpoint;
pub mod contacts;
pub mod grants;
pub mod mcp;
pub mod notifications;
pub mod store;

use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

use crate::arcs::ArcStore;
use crate::calendar::CalendarStore;
use crate::chat::ChatStore;
use crate::contacts::SqliteContactStore;
use crate::grants::GrantStore;
use crate::mcp::McpStore;
use crate::notifications::NotificationStore;
use crate::store::SqliteStore;

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
        let contacts = self.contact_store();
        contacts.init_schema().await?;
        let notifications = self.notification_store();
        notifications.init_schema().await?;
        let mcp = self.mcp_store();
        mcp.init_schema().await?;
        let grants = self.grant_store();
        grants.init_schema().await
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
