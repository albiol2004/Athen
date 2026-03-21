//! Chat message persistence for conversation history across app restarts.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

/// A single persisted chat message.
#[derive(Debug, Clone)]
pub struct PersistedChatMessage {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub content_type: String,
    pub created_at: String,
}

/// Metadata for a chat session displayed in the sidebar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u32,
}

/// SQLite-backed chat message storage.
pub struct ChatStore {
    conn: Arc<Mutex<Connection>>,
}

impl ChatStore {
    /// Create a new `ChatStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the chat_messages table if it does not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(CHAT_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init chat schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Save a chat message to the database.
    pub async fn save_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        content_type: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let role = role.to_string();
        let content = content.to_string();
        let content_type = content_type.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    role,
                    content,
                    content_type,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|e| AthenError::Other(format!("Save chat message: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load all messages for a given session, ordered by creation time.
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<PersistedChatMessage>> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, role, content, content_type, created_at \
                     FROM chat_messages WHERE session_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load messages: {e}")))?;

            let rows = stmt
                .query_map(params![session_id], |row| {
                    Ok(PersistedChatMessage {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        role: row.get(2)?,
                        content: row.get(3)?,
                        content_type: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query messages: {e}")))?;

            let mut messages = Vec::new();
            for row in rows {
                messages.push(
                    row.map_err(|e| AthenError::Other(format!("Message row: {e}")))?,
                );
            }
            Ok(messages)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all distinct session IDs, ordered by the most recent message first.
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT session_id FROM chat_messages \
                     GROUP BY session_id ORDER BY MAX(created_at) DESC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list sessions: {e}")))?;

            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| AthenError::Other(format!("Query sessions: {e}")))?;

            let mut sessions = Vec::new();
            for row in rows {
                sessions
                    .push(row.map_err(|e| AthenError::Other(format!("Session row: {e}")))?);
            }
            Ok(sessions)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete all messages for a given session.
    pub async fn clear_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM chat_messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| AthenError::Other(format!("Clear session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a session metadata entry.
    pub async fn create_session(&self, session_id: &str, name: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT OR IGNORE INTO chat_sessions (session_id, name, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, name, now, now],
            )
            .map_err(|e| AthenError::Other(format!("Create session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Rename a session.
    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE chat_sessions SET name = ?1, updated_at = ?2 WHERE session_id = ?3",
                params![name, now, session_id],
            )
            .map_err(|e| AthenError::Other(format!("Rename session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete a session and all its messages.
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM chat_messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| AthenError::Other(format!("Delete session messages: {e}")))?;
            conn.execute(
                "DELETE FROM chat_sessions WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| AthenError::Other(format!("Delete session meta: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the `updated_at` timestamp for a session.
    pub async fn touch_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE chat_sessions SET updated_at = ?1 WHERE session_id = ?2",
                params![now, session_id],
            )
            .map_err(|e| AthenError::Other(format!("Touch session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all sessions with metadata, ordered by most recently updated first.
    ///
    /// Sessions that exist in `chat_messages` but not in `chat_sessions` are
    /// included with an auto-generated name (migration for existing data).
    pub async fn list_sessions_with_meta(&self) -> Result<Vec<SessionMeta>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Migrate any sessions that exist in messages but not in the
            // metadata table (handles pre-sidebar data).
            conn.execute_batch(
                "INSERT OR IGNORE INTO chat_sessions (session_id, name, created_at, updated_at) \
                 SELECT session_id, session_id, MIN(created_at), MAX(created_at) \
                 FROM chat_messages \
                 WHERE session_id NOT IN (SELECT session_id FROM chat_sessions) \
                 GROUP BY session_id",
            )
            .map_err(|e| AthenError::Other(format!("Migrate sessions: {e}")))?;

            let mut stmt = conn
                .prepare(
                    "SELECT s.session_id, s.name, s.created_at, s.updated_at, \
                            COALESCE(m.cnt, 0) AS message_count \
                     FROM chat_sessions s \
                     LEFT JOIN ( \
                         SELECT session_id, COUNT(*) AS cnt \
                         FROM chat_messages GROUP BY session_id \
                     ) m ON s.session_id = m.session_id \
                     ORDER BY s.updated_at DESC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list sessions meta: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok(SessionMeta {
                        session_id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        message_count: row.get::<_, u32>(4)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query sessions meta: {e}")))?;

            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(
                    row.map_err(|e| AthenError::Other(format!("Session meta row: {e}")))?,
                );
            }
            Ok(sessions)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

const CHAT_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS chat_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    content_type TEXT NOT NULL DEFAULT 'text',
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chat_session ON chat_messages(session_id);

CREATE TABLE IF NOT EXISTS chat_sessions (
    session_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    async fn setup_chat_store() -> ChatStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ChatStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init chat schema");
        store
    }

    #[tokio::test]
    async fn test_save_and_load_messages() {
        let store = setup_chat_store().await;

        store
            .save_message("session_1", "user", "Hello!", "text")
            .await
            .unwrap();
        store
            .save_message("session_1", "assistant", "Hi there!", "text")
            .await
            .unwrap();

        let messages = store.load_messages("session_1").await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello!");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Hi there!");
    }

    #[tokio::test]
    async fn test_load_empty_session() {
        let store = setup_chat_store().await;
        let messages = store.load_messages("nonexistent").await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let store = setup_chat_store().await;

        store
            .save_message("session_a", "user", "First", "text")
            .await
            .unwrap();
        store
            .save_message("session_b", "user", "Second", "text")
            .await
            .unwrap();

        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        // Most recent first
        assert_eq!(sessions[0], "session_b");
        assert_eq!(sessions[1], "session_a");
    }

    #[tokio::test]
    async fn test_clear_session() {
        let store = setup_chat_store().await;

        store
            .save_message("session_1", "user", "Hello", "text")
            .await
            .unwrap();
        store
            .save_message("session_1", "assistant", "World", "text")
            .await
            .unwrap();
        store
            .save_message("session_2", "user", "Other", "text")
            .await
            .unwrap();

        store.clear_session("session_1").await.unwrap();

        let messages_1 = store.load_messages("session_1").await.unwrap();
        assert!(messages_1.is_empty());

        // Other session unaffected
        let messages_2 = store.load_messages("session_2").await.unwrap();
        assert_eq!(messages_2.len(), 1);
    }

    #[tokio::test]
    async fn test_message_ordering() {
        let store = setup_chat_store().await;

        store
            .save_message("s1", "user", "First", "text")
            .await
            .unwrap();
        store
            .save_message("s1", "assistant", "Second", "text")
            .await
            .unwrap();
        store
            .save_message("s1", "user", "Third", "text")
            .await
            .unwrap();

        let messages = store.load_messages("s1").await.unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "First");
        assert_eq!(messages[1].content, "Second");
        assert_eq!(messages[2].content, "Third");
    }

    #[tokio::test]
    async fn test_create_and_list_sessions_with_meta() {
        let store = setup_chat_store().await;

        store
            .create_session("s1", "First Chat")
            .await
            .unwrap();
        store
            .create_session("s2", "Second Chat")
            .await
            .unwrap();

        store
            .save_message("s1", "user", "Hello", "text")
            .await
            .unwrap();
        store
            .save_message("s1", "assistant", "Hi", "text")
            .await
            .unwrap();
        store
            .save_message("s2", "user", "Hey", "text")
            .await
            .unwrap();

        let sessions = store.list_sessions_with_meta().await.unwrap();
        assert_eq!(sessions.len(), 2);

        // Find s1 and s2 regardless of order
        let s1 = sessions.iter().find(|s| s.session_id == "s1").unwrap();
        let s2 = sessions.iter().find(|s| s.session_id == "s2").unwrap();
        assert_eq!(s1.name, "First Chat");
        assert_eq!(s1.message_count, 2);
        assert_eq!(s2.name, "Second Chat");
        assert_eq!(s2.message_count, 1);
    }

    #[tokio::test]
    async fn test_rename_session() {
        let store = setup_chat_store().await;

        store.create_session("s1", "Old Name").await.unwrap();
        store.rename_session("s1", "New Name").await.unwrap();

        let sessions = store.list_sessions_with_meta().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "New Name");
    }

    #[tokio::test]
    async fn test_delete_session() {
        let store = setup_chat_store().await;

        store.create_session("s1", "Chat 1").await.unwrap();
        store.create_session("s2", "Chat 2").await.unwrap();
        store
            .save_message("s1", "user", "Hello", "text")
            .await
            .unwrap();
        store
            .save_message("s2", "user", "World", "text")
            .await
            .unwrap();

        store.delete_session("s1").await.unwrap();

        let sessions = store.list_sessions_with_meta().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s2");

        let messages = store.load_messages("s1").await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_touch_session() {
        let store = setup_chat_store().await;

        store.create_session("s1", "Chat").await.unwrap();
        let before = store.list_sessions_with_meta().await.unwrap();
        let ts1 = before[0].updated_at.clone();

        // Small delay so the timestamp differs.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store.touch_session("s1").await.unwrap();
        let after = store.list_sessions_with_meta().await.unwrap();
        let ts2 = after[0].updated_at.clone();

        assert_ne!(ts1, ts2);
    }

    #[tokio::test]
    async fn test_legacy_sessions_migrated() {
        let store = setup_chat_store().await;

        // Save messages without creating session metadata (simulates pre-sidebar data).
        store
            .save_message("legacy_session", "user", "Old message", "text")
            .await
            .unwrap();

        let sessions = store.list_sessions_with_meta().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "legacy_session");
        assert_eq!(sessions[0].name, "legacy_session");
        assert_eq!(sessions[0].message_count, 1);
    }

    #[tokio::test]
    async fn test_sessions_isolated() {
        let store = setup_chat_store().await;

        store
            .save_message("s1", "user", "Session 1", "text")
            .await
            .unwrap();
        store
            .save_message("s2", "user", "Session 2", "text")
            .await
            .unwrap();

        let m1 = store.load_messages("s1").await.unwrap();
        let m2 = store.load_messages("s2").await.unwrap();

        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].content, "Session 1");
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].content, "Session 2");
    }
}
