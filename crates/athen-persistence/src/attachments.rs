//! SQLite-backed storage for attachment metadata + refetch pointers.
//!
//! Each row carries everything needed to:
//! - find the bytes on disk while they're cached (`local_path`),
//! - find the cached extracted text after the bytes are purged
//!   (`extracted_text_path` outlives `local_path`),
//! - re-download the original bytes from the source server via the
//!   serialised `AttachmentSource` (`source_json` column).
//!
//! The TTL purger walks rows where `purged_at IS NULL AND fetched_at <
//! cutoff`, deletes the file at `local_path`, nulls the path, and
//! stamps `purged_at`. Extracted text + the row itself stay forever
//! (small, useful for arc continuity).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::event::{Attachment, AttachmentId, AttachmentSource};

const ATTACHMENTS_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL,
    name TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    local_path TEXT,
    extracted_text_path TEXT,
    source_json TEXT,
    fetched_at TEXT NOT NULL,
    purged_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_attachments_event ON attachments(event_id);
CREATE INDEX IF NOT EXISTS idx_attachments_purge ON attachments(purged_at, fetched_at);
";

/// SQLite-backed attachment-ref storage.
#[derive(Clone)]
pub struct AttachmentStore {
    conn: Arc<Mutex<Connection>>,
}

impl AttachmentStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the attachments table + indexes if absent.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(ATTACHMENTS_SCHEMA_SQL).map_err(|e| {
                AthenError::Other(format!("Failed to init attachments schema: {e}"))
            })?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Insert a freshly-fetched attachment record. The `event_id` ties
    /// the row to the originating SenseEvent so a single query can list
    /// every attachment for a given message/email.
    pub async fn insert(&self, event_id: Uuid, attachment: &Attachment) -> Result<()> {
        let conn = self.conn.clone();
        let row = AttachmentRow::from_domain(event_id, attachment);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO attachments \
                 (id, event_id, name, mime_type, size_bytes, local_path, \
                  extracted_text_path, source_json, fetched_at, purged_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(id) DO UPDATE SET \
                   local_path = excluded.local_path, \
                   extracted_text_path = excluded.extracted_text_path, \
                   purged_at = excluded.purged_at",
                params![
                    row.id,
                    row.event_id,
                    row.name,
                    row.mime_type,
                    row.size_bytes,
                    row.local_path,
                    row.extracted_text_path,
                    row.source_json,
                    row.fetched_at,
                    row.purged_at,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Insert attachment: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Fetch a single attachment by ID. Returns `None` if no row.
    pub async fn get(&self, id: AttachmentId) -> Result<Option<Attachment>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.query_row(
                "SELECT id, event_id, name, mime_type, size_bytes, local_path, \
                        extracted_text_path, source_json, fetched_at, purged_at \
                   FROM attachments WHERE id = ?1",
                params![id.0.to_string()],
                AttachmentRow::from_row,
            )
            .optional()
            .map_err(|e| AthenError::Other(format!("Get attachment: {e}")))?
            .map(|row| row.into_domain())
            .transpose()
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List every attachment associated with a given SenseEvent.
    pub async fn list_for_event(&self, event_id: Uuid) -> Result<Vec<Attachment>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, event_id, name, mime_type, size_bytes, local_path, \
                            extracted_text_path, source_json, fetched_at, purged_at \
                       FROM attachments WHERE event_id = ?1 ORDER BY fetched_at",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list_for_event: {e}")))?;
            let rows = stmt
                .query_map(params![event_id.to_string()], AttachmentRow::from_row)
                .map_err(|e| AthenError::Other(format!("Query list_for_event: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                let row = row
                    .map_err(|e| AthenError::Other(format!("Row decode: {e}")))?;
                out.push(row.into_domain()?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// IDs and on-disk paths of attachments older than `cutoff` whose
    /// bytes haven't been purged yet. The caller deletes the files and
    /// then calls [`mark_purged`].
    pub async fn list_purgeable(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<(AttachmentId, PathBuf)>> {
        let conn = self.conn.clone();
        let cutoff_str = cutoff.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, local_path FROM attachments \
                       WHERE purged_at IS NULL \
                         AND local_path IS NOT NULL \
                         AND fetched_at < ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list_purgeable: {e}")))?;
            let rows = stmt
                .query_map(params![cutoff_str], |row| {
                    let id: String = row.get(0)?;
                    let path: String = row.get(1)?;
                    Ok((id, path))
                })
                .map_err(|e| AthenError::Other(format!("Query list_purgeable: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, path) = row
                    .map_err(|e| AthenError::Other(format!("Row decode: {e}")))?;
                let id = Uuid::parse_str(&id)
                    .map_err(|e| AthenError::Other(format!("Bad attachment id: {e}")))?;
                out.push((AttachmentId(id), PathBuf::from(path)));
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Mark an attachment as purged: null `local_path`, stamp
    /// `purged_at`. The row + extracted_text_path remain so the agent
    /// can still recall what the file said and re-download via
    /// `source_json` if needed.
    pub async fn mark_purged(&self, id: AttachmentId) -> Result<()> {
        let conn = self.conn.clone();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE attachments \
                    SET local_path = NULL, purged_at = ?1 \
                  WHERE id = ?2",
                params![now, id.0.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Mark purged: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the `local_path` after a refetch + the `purged_at` clear.
    /// Lets the agent re-download a previously-purged attachment and
    /// have subsequent `get`s see it as live again.
    pub async fn record_refetch(&self, id: AttachmentId, local_path: PathBuf) -> Result<()> {
        let conn = self.conn.clone();
        let path_str = local_path.to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE attachments \
                    SET local_path = ?1, purged_at = NULL \
                  WHERE id = ?2",
                params![path_str, id.0.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Record refetch: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the cached extracted-text sidecar path. Called after
    /// pdf-extract runs.
    pub async fn record_extracted_text(
        &self,
        id: AttachmentId,
        path: PathBuf,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let path_str = path.to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE attachments \
                    SET extracted_text_path = ?1 \
                  WHERE id = ?2",
                params![path_str, id.0.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Record extracted text: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

/// Mirrors the row shape — keeps the SQL → struct mapping local.
struct AttachmentRow {
    id: String,
    event_id: String,
    name: String,
    mime_type: String,
    size_bytes: i64,
    local_path: Option<String>,
    extracted_text_path: Option<String>,
    source_json: Option<String>,
    fetched_at: String,
    purged_at: Option<String>,
}

impl AttachmentRow {
    fn from_domain(event_id: Uuid, a: &Attachment) -> Self {
        Self {
            id: a.id.0.to_string(),
            event_id: event_id.to_string(),
            name: a.name.clone(),
            mime_type: a.mime_type.clone(),
            size_bytes: a.size_bytes as i64,
            local_path: a
                .local_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            extracted_text_path: a
                .extracted_text_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            source_json: a
                .source
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default()),
            fetched_at: a.fetched_at.to_rfc3339(),
            purged_at: a.purged_at.map(|t| t.to_rfc3339()),
        }
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            event_id: row.get(1)?,
            name: row.get(2)?,
            mime_type: row.get(3)?,
            size_bytes: row.get(4)?,
            local_path: row.get(5)?,
            extracted_text_path: row.get(6)?,
            source_json: row.get(7)?,
            fetched_at: row.get(8)?,
            purged_at: row.get(9)?,
        })
    }

    fn into_domain(self) -> Result<Attachment> {
        let id = Uuid::parse_str(&self.id)
            .map_err(|e| AthenError::Other(format!("Bad attachment uuid: {e}")))?;
        let fetched_at = DateTime::parse_from_rfc3339(&self.fetched_at)
            .map_err(|e| AthenError::Other(format!("Bad fetched_at: {e}")))?
            .with_timezone(&Utc);
        let purged_at = self
            .purged_at
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| AthenError::Other(format!("Bad purged_at: {e}")))
            })
            .transpose()?;
        let source = self
            .source_json
            .map(|s| {
                serde_json::from_str::<AttachmentSource>(&s)
                    .map_err(|e| AthenError::Other(format!("Bad source_json: {e}")))
            })
            .transpose()?;

        Ok(Attachment {
            id: AttachmentId(id),
            name: self.name,
            mime_type: self.mime_type,
            size_bytes: self.size_bytes as u64,
            local_path: self.local_path.map(PathBuf::from),
            extracted_text_path: self.extracted_text_path.map(PathBuf::from),
            source,
            fetched_at,
            purged_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    fn sample(local_path: Option<&str>) -> Attachment {
        Attachment::new(
            "invoice.pdf",
            "application/pdf",
            12_345,
            local_path.map(PathBuf::from),
            Some(AttachmentSource::Email {
                account_id: "primary".into(),
                mailbox: "INBOX".into(),
                uid_validity: 1,
                uid: 42,
                part_path: "2.1".into(),
            }),
        )
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();

        let event_id = Uuid::new_v4();
        let a = sample(Some("/tmp/inv.pdf"));
        store.insert(event_id, &a).await.unwrap();

        let fetched = store.get(a.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "invoice.pdf");
        assert_eq!(fetched.size_bytes, 12_345);
        assert!(fetched.is_local());
        assert!(matches!(fetched.source, Some(AttachmentSource::Email { .. })));
    }

    #[tokio::test]
    async fn list_for_event_returns_only_matching() {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        store.insert(e1, &sample(None)).await.unwrap();
        store.insert(e1, &sample(None)).await.unwrap();
        store.insert(e2, &sample(None)).await.unwrap();

        let only_e1 = store.list_for_event(e1).await.unwrap();
        assert_eq!(only_e1.len(), 2);
    }

    #[tokio::test]
    async fn purge_marks_and_lists() {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();

        let event_id = Uuid::new_v4();
        let a = sample(Some("/tmp/foo.pdf"));
        store.insert(event_id, &a).await.unwrap();

        // Cutoff in the future: row qualifies as purgeable.
        let purgeable = store.list_purgeable(Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(purgeable.len(), 1);
        assert_eq!(purgeable[0].0, a.id);

        store.mark_purged(a.id).await.unwrap();

        // After purge, no longer in purgeable list (local_path is null).
        let after = store.list_purgeable(Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert!(after.is_empty());

        let row = store.get(a.id).await.unwrap().unwrap();
        assert!(row.is_purged());
        assert!(row.local_path.is_none());
    }

    #[tokio::test]
    async fn refetch_clears_purged() {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();

        let event_id = Uuid::new_v4();
        let a = sample(Some("/tmp/foo.pdf"));
        store.insert(event_id, &a).await.unwrap();
        store.mark_purged(a.id).await.unwrap();

        store
            .record_refetch(a.id, PathBuf::from("/tmp/refetch.pdf"))
            .await
            .unwrap();
        let row = store.get(a.id).await.unwrap().unwrap();
        assert!(row.is_local());
        assert_eq!(row.local_path.unwrap().to_str().unwrap(), "/tmp/refetch.pdf");
    }

    #[tokio::test]
    async fn record_extracted_text_persists() {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();

        let event_id = Uuid::new_v4();
        let a = sample(Some("/tmp/foo.pdf"));
        store.insert(event_id, &a).await.unwrap();

        store
            .record_extracted_text(a.id, PathBuf::from("/tmp/foo.pdf.txt"))
            .await
            .unwrap();
        let row = store.get(a.id).await.unwrap().unwrap();
        assert_eq!(
            row.extracted_text_path.unwrap().to_str().unwrap(),
            "/tmp/foo.pdf.txt"
        );
    }
}
