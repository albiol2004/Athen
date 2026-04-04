//! Arc persistence for branch-like conversation workflows.
//!
//! Arcs replace "chat sessions" with a richer model that supports branching
//! (parent arcs) and merging, similar to git branches.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

/// The source that originated an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArcSource {
    UserInput,
    Email,
    Calendar,
    Messaging,
    System,
}

impl ArcSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserInput => "user_input",
            Self::Email => "email",
            Self::Calendar => "calendar",
            Self::Messaging => "messaging",
            Self::System => "system",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "email" => Self::Email,
            "calendar" => Self::Calendar,
            "messaging" => Self::Messaging,
            "system" => Self::System,
            _ => Self::UserInput,
        }
    }

    /// Icon character for the UI sidebar.
    pub fn icon(&self) -> &str {
        match self {
            Self::UserInput => "\u{1f4ac}",
            Self::Email => "\u{1f4e7}",
            Self::Calendar => "\u{1f4c5}",
            Self::Messaging => "\u{1f4ac}",
            Self::System => "\u{2699}\u{fe0f}",
        }
    }
}

/// Status of an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArcStatus {
    Active,
    Archived,
    Merged,
}

impl ArcStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Merged => "merged",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "archived" => Self::Archived,
            "merged" => Self::Merged,
            _ => Self::Active,
        }
    }
}

/// Metadata for an Arc displayed in the sidebar.
#[derive(Debug, Clone, Serialize)]
pub struct ArcMeta {
    pub id: String,
    pub name: String,
    pub source: ArcSource,
    pub status: ArcStatus,
    pub parent_arc_id: Option<String>,
    pub merged_into_arc_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub entry_count: u32,
}

/// The type of an entry within an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
    Message,
    ToolCall,
    EmailEvent,
    CalendarEvent,
    SystemEvent,
}

impl EntryType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::EmailEvent => "email_event",
            Self::CalendarEvent => "calendar_event",
            Self::SystemEvent => "system_event",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "tool_call" => Self::ToolCall,
            "email_event" => Self::EmailEvent,
            "calendar_event" => Self::CalendarEvent,
            "system_event" => Self::SystemEvent,
            _ => Self::Message,
        }
    }
}

/// A single entry in an Arc.
#[derive(Debug, Clone, Serialize)]
pub struct ArcEntry {
    pub id: i64,
    pub arc_id: String,
    pub entry_type: EntryType,
    pub source: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
}

/// SQLite-backed Arc storage.
pub struct ArcStore {
    conn: Arc<Mutex<Connection>>,
}

impl ArcStore {
    /// Create a new `ArcStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the arcs and arc_entries tables if they do not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(ARC_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init arc schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new Arc with Active status.
    pub async fn create_arc(&self, id: &str, name: &str, source: ArcSource) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        let source_str = source.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arcs (id, name, source, status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, name, source_str, "active", now, now],
            )
            .map_err(|e| AthenError::Other(format!("Create arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new Arc branched from a parent Arc.
    pub async fn create_arc_with_parent(
        &self,
        id: &str,
        name: &str,
        source: ArcSource,
        parent_arc_id: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        let source_str = source.as_str().to_string();
        let parent_arc_id = parent_arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arcs (id, name, source, status, parent_arc_id, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, name, source_str, "active", parent_arc_id, now, now],
            )
            .map_err(|e| AthenError::Other(format!("Create arc with parent: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Get a single Arc by ID.
    pub async fn get_arc(&self, id: &str) -> Result<Option<ArcMeta>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT a.id, a.name, a.source, a.status, a.parent_arc_id, \
                            a.merged_into_arc_id, a.created_at, a.updated_at, \
                            COALESCE(e.cnt, 0) AS entry_count \
                     FROM arcs a \
                     LEFT JOIN ( \
                         SELECT arc_id, COUNT(*) AS cnt \
                         FROM arc_entries GROUP BY arc_id \
                     ) e ON a.id = e.arc_id \
                     WHERE a.id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare get arc: {e}")))?;

            let mut rows = stmt
                .query_map(params![id], |row| {
                    Ok(ArcMeta {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: ArcSource::from_str(&row.get::<_, String>(2)?),
                        status: ArcStatus::from_str(&row.get::<_, String>(3)?),
                        parent_arc_id: row.get(4)?,
                        merged_into_arc_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        entry_count: row.get::<_, u32>(8)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query get arc: {e}")))?;

            match rows.next() {
                Some(row) => {
                    let meta =
                        row.map_err(|e| AthenError::Other(format!("Get arc row: {e}")))?;
                    Ok(Some(meta))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all arcs ordered by most recently updated first, with entry counts.
    pub async fn list_arcs(&self) -> Result<Vec<ArcMeta>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT a.id, a.name, a.source, a.status, a.parent_arc_id, \
                            a.merged_into_arc_id, a.created_at, a.updated_at, \
                            COALESCE(e.cnt, 0) AS entry_count \
                     FROM arcs a \
                     LEFT JOIN ( \
                         SELECT arc_id, COUNT(*) AS cnt \
                         FROM arc_entries GROUP BY arc_id \
                     ) e ON a.id = e.arc_id \
                     ORDER BY a.updated_at DESC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list arcs: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok(ArcMeta {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: ArcSource::from_str(&row.get::<_, String>(2)?),
                        status: ArcStatus::from_str(&row.get::<_, String>(3)?),
                        parent_arc_id: row.get(4)?,
                        merged_into_arc_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        entry_count: row.get::<_, u32>(8)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query list arcs: {e}")))?;

            let mut arcs = Vec::new();
            for row in rows {
                arcs.push(
                    row.map_err(|e| AthenError::Other(format!("List arcs row: {e}")))?,
                );
            }
            Ok(arcs)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Rename an arc.
    pub async fn rename_arc(&self, id: &str, name: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![name, now, id],
            )
            .map_err(|e| AthenError::Other(format!("Rename arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete an arc and all its entries.
    pub async fn delete_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM arc_entries WHERE arc_id = ?1",
                params![id],
            )
            .map_err(|e| AthenError::Other(format!("Delete arc entries: {e}")))?;
            conn.execute("DELETE FROM arcs WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Archive an arc by setting its status to Archived.
    pub async fn archive_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params!["archived", now, id],
            )
            .map_err(|e| AthenError::Other(format!("Archive arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Merge all entries from source arc into target arc.
    ///
    /// Moves entries by updating their `arc_id`, sets the source arc status to
    /// Merged with `merged_into_arc_id` pointing to the target, and touches the
    /// target's `updated_at`.
    pub async fn merge_arc(&self, source_id: &str, target_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let source_id = source_id.to_string();
        let target_id = target_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();

            // Move all entries from source to target.
            conn.execute(
                "UPDATE arc_entries SET arc_id = ?1 WHERE arc_id = ?2",
                params![target_id, source_id],
            )
            .map_err(|e| AthenError::Other(format!("Merge arc entries: {e}")))?;

            // Mark source as merged.
            conn.execute(
                "UPDATE arcs SET status = ?1, merged_into_arc_id = ?2, updated_at = ?3 \
                 WHERE id = ?4",
                params!["merged", target_id, now, source_id],
            )
            .map_err(|e| AthenError::Other(format!("Mark source arc merged: {e}")))?;

            // Touch target's updated_at.
            conn.execute(
                "UPDATE arcs SET updated_at = ?1 WHERE id = ?2",
                params![now, target_id],
            )
            .map_err(|e| AthenError::Other(format!("Touch target arc: {e}")))?;

            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the `updated_at` timestamp for an arc.
    pub async fn touch_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(|e| AthenError::Other(format!("Touch arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Add an entry to an arc. Returns the entry's auto-generated ID.
    pub async fn add_entry(
        &self,
        arc_id: &str,
        entry_type: EntryType,
        source: &str,
        content: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let entry_type_str = entry_type.as_str().to_string();
        let source = source.to_string();
        let content = content.to_string();
        let metadata_str = metadata.map(|v| v.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arc_entries (arc_id, entry_type, source, content, metadata, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![arc_id, entry_type_str, source, content, metadata_str, now],
            )
            .map_err(|e| AthenError::Other(format!("Add arc entry: {e}")))?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load all entries for an arc, ordered by ID ascending.
    pub async fn load_entries(&self, arc_id: &str) -> Result<Vec<ArcEntry>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, arc_id, entry_type, source, content, metadata, created_at \
                     FROM arc_entries WHERE arc_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load entries: {e}")))?;

            let rows = stmt
                .query_map(params![arc_id], |row| {
                    let metadata_str: Option<String> = row.get(5)?;
                    let metadata = metadata_str
                        .and_then(|s| serde_json::from_str(&s).ok());
                    Ok(ArcEntry {
                        id: row.get(0)?,
                        arc_id: row.get(1)?,
                        entry_type: EntryType::from_str(&row.get::<_, String>(2)?),
                        source: row.get(3)?,
                        content: row.get(4)?,
                        metadata,
                        created_at: row.get(6)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query load entries: {e}")))?;

            let mut entries = Vec::new();
            for row in rows {
                entries.push(
                    row.map_err(|e| AthenError::Other(format!("Entry row: {e}")))?,
                );
            }
            Ok(entries)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Migrate data from legacy `chat_sessions` and `chat_messages` tables into arcs.
    ///
    /// Returns the number of arcs migrated. Idempotent: skips migration if arcs
    /// already contain data.
    pub async fn migrate_from_chat_tables(&self) -> Result<u32> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Check if arcs already have data — skip if so.
            let arc_count: u32 = conn
                .query_row("SELECT COUNT(*) FROM arcs", [], |row| row.get(0))
                .map_err(|e| AthenError::Other(format!("Count arcs: {e}")))?;
            if arc_count > 0 {
                return Ok(0);
            }

            // Check if old tables exist.
            let has_chat_sessions: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type='table' AND name='chat_sessions'",
                    [],
                    |row| row.get::<_, u32>(0).map(|c| c > 0),
                )
                .map_err(|e| AthenError::Other(format!("Check chat_sessions: {e}")))?;

            let has_chat_messages: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type='table' AND name='chat_messages'",
                    [],
                    |row| row.get::<_, u32>(0).map(|c| c > 0),
                )
                .map_err(|e| AthenError::Other(format!("Check chat_messages: {e}")))?;

            if !has_chat_messages {
                return Ok(0);
            }

            let now = Utc::now().to_rfc3339();
            let mut migrated: u32 = 0;

            // Collect session IDs from chat_messages.
            let mut sess_stmt = if has_chat_sessions {
                conn.prepare(
                    "SELECT s.session_id, COALESCE(s.name, s.session_id), \
                            COALESCE(s.created_at, ?1), COALESCE(s.updated_at, ?1) \
                     FROM chat_sessions s",
                )
            } else {
                conn.prepare(
                    "SELECT session_id, session_id, MIN(created_at), MAX(created_at) \
                     FROM chat_messages GROUP BY session_id",
                )
            }
            .map_err(|e| AthenError::Other(format!("Prepare migrate sessions: {e}")))?;

            // Gather sessions into a vec to avoid borrow issues.
            let sessions: Vec<(String, String, String, String)> = if has_chat_sessions {
                let rows = sess_stmt
                    .query_map(params![now], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| AthenError::Other(format!("Query migrate sessions: {e}")))?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(
                        row.map_err(|e| {
                            AthenError::Other(format!("Migrate session row: {e}"))
                        })?,
                    );
                }
                v
            } else {
                let rows = sess_stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| AthenError::Other(format!("Query migrate sessions: {e}")))?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(
                        row.map_err(|e| {
                            AthenError::Other(format!("Migrate session row: {e}"))
                        })?,
                    );
                }
                v
            };

            for (session_id, name, created_at, updated_at) in &sessions {
                // Create arc for this session.
                conn.execute(
                    "INSERT INTO arcs (id, name, source, status, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session_id, name, "user_input", "active", created_at, updated_at],
                )
                .map_err(|e| AthenError::Other(format!("Insert migrated arc: {e}")))?;

                // Migrate messages as entries.
                let mut msg_stmt = conn
                    .prepare(
                        "SELECT role, content, created_at \
                         FROM chat_messages WHERE session_id = ?1 ORDER BY id ASC",
                    )
                    .map_err(|e| AthenError::Other(format!("Prepare migrate messages: {e}")))?;

                let messages: Vec<(String, String, String)> = {
                    let rows = msg_stmt
                        .query_map(params![session_id], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })
                        .map_err(|e| {
                            AthenError::Other(format!("Query migrate messages: {e}"))
                        })?;
                    let mut v = Vec::new();
                    for row in rows {
                        v.push(row.map_err(|e| {
                            AthenError::Other(format!("Migrate message row: {e}"))
                        })?);
                    }
                    v
                };

                for (role, content, msg_created_at) in &messages {
                    conn.execute(
                        "INSERT INTO arc_entries \
                         (arc_id, entry_type, source, content, created_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![session_id, "message", role, content, msg_created_at],
                    )
                    .map_err(|e| AthenError::Other(format!("Insert migrated entry: {e}")))?;
                }

                migrated += 1;
            }

            Ok(migrated)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

const ARC_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS arcs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'user_input',
    status TEXT NOT NULL DEFAULT 'active',
    parent_arc_id TEXT,
    merged_into_arc_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS arc_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    arc_id TEXT NOT NULL,
    entry_type TEXT NOT NULL DEFAULT 'message',
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (arc_id) REFERENCES arcs(id)
);

CREATE INDEX IF NOT EXISTS idx_arc_entries_arc ON arc_entries(arc_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    async fn setup_arc_store() -> ArcStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ArcStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init arc schema");
        store
    }

    /// Helper that creates an ArcStore with the chat schema also initialized,
    /// and returns both the store and the raw connection for inserting legacy data.
    async fn setup_arc_store_with_chat() -> (ArcStore, Arc<Mutex<Connection>>) {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        // Init chat schema via spawn_blocking to avoid blocking the runtime.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS chat_sessions (
                    session_id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS chat_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    content_type TEXT NOT NULL DEFAULT 'text',
                    created_at TEXT NOT NULL
                );",
            )
            .expect("init chat schema");
        })
        .await
        .expect("spawn blocking");

        let store = ArcStore::new(conn.clone());
        store.init_schema().await.expect("init arc schema");
        (store, conn)
    }

    #[tokio::test]
    async fn test_create_and_list_arcs() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "First Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("arc_2", "Email Arc", ArcSource::Email)
            .await
            .unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 2);

        let a1 = arcs.iter().find(|a| a.id == "arc_1").unwrap();
        assert_eq!(a1.name, "First Arc");
        assert_eq!(a1.source, ArcSource::UserInput);
        assert_eq!(a1.status, ArcStatus::Active);
        assert!(a1.parent_arc_id.is_none());
        assert_eq!(a1.entry_count, 0);

        let a2 = arcs.iter().find(|a| a.id == "arc_2").unwrap();
        assert_eq!(a2.name, "Email Arc");
        assert_eq!(a2.source, ArcSource::Email);
    }

    #[tokio::test]
    async fn test_add_and_load_entries() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Test Arc", ArcSource::UserInput)
            .await
            .unwrap();

        store
            .add_entry("arc_1", EntryType::Message, "user", "Hello!", None)
            .await
            .unwrap();
        store
            .add_entry("arc_1", EntryType::ToolCall, "assistant", "Running ls", None)
            .await
            .unwrap();
        store
            .add_entry("arc_1", EntryType::Message, "assistant", "Done!", None)
            .await
            .unwrap();

        let entries = store.load_entries("arc_1").await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[0].source, "user");
        assert_eq!(entries[0].content, "Hello!");
        assert_eq!(entries[1].entry_type, EntryType::ToolCall);
        assert_eq!(entries[1].content, "Running ls");
        assert_eq!(entries[2].content, "Done!");
    }

    #[tokio::test]
    async fn test_rename_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Old Name", ArcSource::UserInput)
            .await
            .unwrap();
        store.rename_arc("arc_1", "New Name").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap().unwrap();
        assert_eq!(arc.name, "New Name");
    }

    #[tokio::test]
    async fn test_delete_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Doomed", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("arc_1", EntryType::Message, "user", "Hello", None)
            .await
            .unwrap();

        store.delete_arc("arc_1").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap();
        assert!(arc.is_none());

        let entries = store.load_entries("arc_1").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_archive_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Active Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store.archive_arc("arc_1").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap().unwrap();
        assert_eq!(arc.status, ArcStatus::Archived);
    }

    #[tokio::test]
    async fn test_branch_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("parent", "Parent Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc_with_parent("child", "Child Arc", ArcSource::UserInput, "parent")
            .await
            .unwrap();

        let child = store.get_arc("child").await.unwrap().unwrap();
        assert_eq!(child.parent_arc_id, Some("parent".to_string()));
        assert_eq!(child.status, ArcStatus::Active);

        let parent = store.get_arc("parent").await.unwrap().unwrap();
        assert!(parent.parent_arc_id.is_none());
    }

    #[tokio::test]
    async fn test_merge_arcs() {
        let store = setup_arc_store().await;

        store
            .create_arc("source", "Source Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("target", "Target Arc", ArcSource::UserInput)
            .await
            .unwrap();

        store
            .add_entry("source", EntryType::Message, "user", "From source 1", None)
            .await
            .unwrap();
        store
            .add_entry("source", EntryType::Message, "user", "From source 2", None)
            .await
            .unwrap();
        store
            .add_entry("target", EntryType::Message, "user", "In target", None)
            .await
            .unwrap();

        store.merge_arc("source", "target").await.unwrap();

        // Source should be marked Merged.
        let source = store.get_arc("source").await.unwrap().unwrap();
        assert_eq!(source.status, ArcStatus::Merged);
        assert_eq!(source.merged_into_arc_id, Some("target".to_string()));

        // Source entries should be empty.
        let source_entries = store.load_entries("source").await.unwrap();
        assert!(source_entries.is_empty());

        // Target should have all 3 entries.
        let target_entries = store.load_entries("target").await.unwrap();
        assert_eq!(target_entries.len(), 3);

        // Target entry_count should reflect the merge.
        let target = store.get_arc("target").await.unwrap().unwrap();
        assert_eq!(target.entry_count, 3);
    }

    #[tokio::test]
    async fn test_touch_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Test", ArcSource::UserInput)
            .await
            .unwrap();
        let before = store.get_arc("arc_1").await.unwrap().unwrap();
        let ts1 = before.updated_at.clone();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store.touch_arc("arc_1").await.unwrap();
        let after = store.get_arc("arc_1").await.unwrap().unwrap();
        let ts2 = after.updated_at.clone();

        assert_ne!(ts1, ts2);
    }

    #[tokio::test]
    async fn test_entry_metadata() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Meta Arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = json!({
            "tool": "shell_execute",
            "args": {"command": "ls -la"},
            "exit_code": 0
        });
        store
            .add_entry(
                "arc_1",
                EntryType::ToolCall,
                "assistant",
                "Executed ls",
                Some(meta.clone()),
            )
            .await
            .unwrap();

        let entries = store.load_entries("arc_1").await.unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.entry_type, EntryType::ToolCall);

        let loaded_meta = entry.metadata.as_ref().unwrap();
        assert_eq!(loaded_meta["tool"], "shell_execute");
        assert_eq!(loaded_meta["args"]["command"], "ls -la");
        assert_eq!(loaded_meta["exit_code"], 0);
    }

    #[tokio::test]
    async fn test_migrate_from_chat() {
        let (store, conn) = setup_arc_store_with_chat().await;

        // Insert legacy chat data via raw SQL in spawn_blocking.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute(
                "INSERT INTO chat_sessions (session_id, name, created_at, updated_at) \
                 VALUES ('s1', 'Chat One', '2025-01-01T00:00:00Z', '2025-01-01T01:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_sessions (session_id, name, created_at, updated_at) \
                 VALUES ('s2', 'Chat Two', '2025-01-02T00:00:00Z', '2025-01-02T01:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s1', 'user', 'Hello', 'text', '2025-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s1', 'assistant', 'Hi!', 'text', '2025-01-01T00:01:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s2', 'user', 'Hey', 'text', '2025-01-02T00:00:00Z')",
                [],
            )
            .unwrap();
        })
        .await
        .unwrap();

        let count = store.migrate_from_chat_tables().await.unwrap();
        assert_eq!(count, 2);

        // Verify arcs were created.
        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 2);

        let a1 = arcs.iter().find(|a| a.id == "s1").unwrap();
        assert_eq!(a1.name, "Chat One");
        assert_eq!(a1.source, ArcSource::UserInput);
        assert_eq!(a1.entry_count, 2);

        let a2 = arcs.iter().find(|a| a.id == "s2").unwrap();
        assert_eq!(a2.name, "Chat Two");
        assert_eq!(a2.entry_count, 1);

        // Verify entries.
        let entries = store.load_entries("s1").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "user");
        assert_eq!(entries[0].content, "Hello");
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[1].source, "assistant");
        assert_eq!(entries[1].content, "Hi!");

        // Verify idempotency — second call should migrate 0.
        let count2 = store.migrate_from_chat_tables().await.unwrap();
        assert_eq!(count2, 0);
    }

    #[tokio::test]
    async fn test_merge_moves_all_entries() {
        let store = setup_arc_store().await;
        store.create_arc("src", "Source", ArcSource::Email).await.unwrap();
        store.create_arc("tgt", "Target", ArcSource::UserInput).await.unwrap();

        // Add entries to source
        store.add_entry("src", EntryType::EmailEvent, "email", "Email 1", None).await.unwrap();
        store.add_entry("src", EntryType::EmailEvent, "email", "Email 2", None).await.unwrap();
        store.add_entry("tgt", EntryType::Message, "user", "Hello", None).await.unwrap();

        // Merge source into target
        store.merge_arc("src", "tgt").await.unwrap();

        // Target should have all 3 entries
        let target_entries = store.load_entries("tgt").await.unwrap();
        assert_eq!(target_entries.len(), 3);

        // Source should have 0 entries
        let source_entries = store.load_entries("src").await.unwrap();
        assert_eq!(source_entries.len(), 0);

        // Source arc should be Merged
        let src_arc = store.get_arc("src").await.unwrap().unwrap();
        assert_eq!(src_arc.status, ArcStatus::Merged);
        assert_eq!(src_arc.merged_into_arc_id.as_deref(), Some("tgt"));
    }

    #[tokio::test]
    async fn test_merged_arcs_not_in_active_list() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "Active", ArcSource::UserInput).await.unwrap();
        store.create_arc("a2", "Will Merge", ArcSource::Email).await.unwrap();
        store.merge_arc("a2", "a1").await.unwrap();

        let arcs = store.list_arcs().await.unwrap();
        // Both are still listed (filtering by status is done at the app layer)
        let merged = arcs.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(merged.status, ArcStatus::Merged);
    }

    #[tokio::test]
    async fn test_entry_types_preserved() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "Multi-type", ArcSource::UserInput).await.unwrap();

        store.add_entry("a1", EntryType::Message, "user", "Hello", None).await.unwrap();
        store.add_entry("a1", EntryType::ToolCall, "assistant", "shell_execute echo hi", None).await.unwrap();
        store.add_entry("a1", EntryType::EmailEvent, "email", "From: bob", None).await.unwrap();
        store.add_entry("a1", EntryType::SystemEvent, "system", "Monitor started", None).await.unwrap();
        store.add_entry("a1", EntryType::CalendarEvent, "calendar", "Meeting at 3pm", None).await.unwrap();

        let entries = store.load_entries("a1").await.unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[1].entry_type, EntryType::ToolCall);
        assert_eq!(entries[2].entry_type, EntryType::EmailEvent);
        assert_eq!(entries[3].entry_type, EntryType::SystemEvent);
        assert_eq!(entries[4].entry_type, EntryType::CalendarEvent);
    }

    #[tokio::test]
    async fn test_entry_metadata_json() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "Meta test", ArcSource::Email).await.unwrap();

        let meta = serde_json::json!({
            "event_id": "uuid-123",
            "sender": "alice@test.com",
            "relevance": "high",
            "nested": {"key": "value"}
        });

        store.add_entry("a1", EntryType::EmailEvent, "email", "Content", Some(meta.clone())).await.unwrap();

        let entries = store.load_entries("a1").await.unwrap();
        assert_eq!(entries.len(), 1);
        let loaded_meta = entries[0].metadata.as_ref().unwrap();
        assert_eq!(loaded_meta["sender"], "alice@test.com");
        assert_eq!(loaded_meta["relevance"], "high");
        assert_eq!(loaded_meta["nested"]["key"], "value");
    }

    #[tokio::test]
    async fn test_branch_preserves_parent_reference() {
        let store = setup_arc_store().await;
        store.create_arc("parent", "Parent Arc", ArcSource::UserInput).await.unwrap();
        store.add_entry("parent", EntryType::Message, "user", "Original message", None).await.unwrap();

        store.create_arc_with_parent("child", "Branch", ArcSource::UserInput, "parent").await.unwrap();
        store.add_entry("child", EntryType::Message, "user", "Branch message", None).await.unwrap();

        let child = store.get_arc("child").await.unwrap().unwrap();
        assert_eq!(child.parent_arc_id.as_deref(), Some("parent"));
        assert_eq!(child.status, ArcStatus::Active);

        // Parent should be unaffected
        let parent = store.get_arc("parent").await.unwrap().unwrap();
        assert_eq!(parent.status, ArcStatus::Active);
        let parent_entries = store.load_entries("parent").await.unwrap();
        assert_eq!(parent_entries.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_arc_cascades_entries() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "To Delete", ArcSource::UserInput).await.unwrap();
        store.add_entry("a1", EntryType::Message, "user", "Msg 1", None).await.unwrap();
        store.add_entry("a1", EntryType::Message, "assistant", "Msg 2", None).await.unwrap();
        store.add_entry("a1", EntryType::ToolCall, "assistant", "Tool call", None).await.unwrap();

        store.delete_arc("a1").await.unwrap();

        assert!(store.get_arc("a1").await.unwrap().is_none());
        assert!(store.load_entries("a1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_archive_arc_status_change() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "To Archive", ArcSource::UserInput).await.unwrap();
        store.archive_arc("a1").await.unwrap();

        let arc = store.get_arc("a1").await.unwrap().unwrap();
        assert_eq!(arc.status, ArcStatus::Archived);
    }

    #[tokio::test]
    async fn test_multiple_arcs_different_sources() {
        let store = setup_arc_store().await;
        store.create_arc("chat1", "Chat", ArcSource::UserInput).await.unwrap();
        store.create_arc("email1", "Email thread", ArcSource::Email).await.unwrap();
        store.create_arc("cal1", "Meeting", ArcSource::Calendar).await.unwrap();
        store.create_arc("msg1", "WhatsApp", ArcSource::Messaging).await.unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 4);

        let sources: Vec<&ArcSource> = arcs.iter().map(|a| &a.source).collect();
        assert!(sources.contains(&&ArcSource::UserInput));
        assert!(sources.contains(&&ArcSource::Email));
        assert!(sources.contains(&&ArcSource::Calendar));
        assert!(sources.contains(&&ArcSource::Messaging));
    }

    #[tokio::test]
    async fn test_add_entry_returns_id() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "Test", ArcSource::UserInput).await.unwrap();

        let id1 = store.add_entry("a1", EntryType::Message, "user", "First", None).await.unwrap();
        let id2 = store.add_entry("a1", EntryType::Message, "user", "Second", None).await.unwrap();

        assert!(id2 > id1);
    }

    #[tokio::test]
    async fn test_arc_entry_count_updates() {
        let store = setup_arc_store().await;
        store.create_arc("a1", "Count test", ArcSource::UserInput).await.unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs[0].entry_count, 0);

        store.add_entry("a1", EntryType::Message, "user", "One", None).await.unwrap();
        store.add_entry("a1", EntryType::Message, "user", "Two", None).await.unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs[0].entry_count, 2);
    }
}
