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
