//! SQLite-backed notification storage for Athen's notification system.
//!
//! Notifications are stored in a single table with read/unread tracking.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};

const NOTIFICATIONS_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS notifications (
    id TEXT PRIMARY KEY,
    urgency TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    origin TEXT NOT NULL,
    arc_id TEXT,
    task_id TEXT,
    requires_response INTEGER NOT NULL DEFAULT 0,
    is_read INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

/// SQLite-backed notification storage.
#[derive(Clone)]
pub struct NotificationStore {
    conn: Arc<Mutex<Connection>>,
}

impl NotificationStore {
    /// Create a new `NotificationStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the notifications table if it does not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(NOTIFICATIONS_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init notifications schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Save a notification (INSERT OR REPLACE).
    pub async fn save(&self, notification: &Notification, is_read: bool) -> Result<()> {
        let conn = self.conn.clone();
        let notification = notification.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            let task_id_str = notification.task_id.map(|id| id.to_string());

            conn.execute(
                "INSERT INTO notifications \
                 (id, urgency, title, body, origin, arc_id, task_id, \
                  requires_response, is_read, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                 ON CONFLICT(id) DO UPDATE SET \
                  urgency = excluded.urgency, \
                  title = excluded.title, \
                  body = excluded.body, \
                  origin = excluded.origin, \
                  arc_id = excluded.arc_id, \
                  task_id = excluded.task_id, \
                  requires_response = excluded.requires_response, \
                  is_read = excluded.is_read, \
                  updated_at = excluded.updated_at",
                params![
                    notification.id.to_string(),
                    urgency_to_str(&notification.urgency),
                    notification.title,
                    notification.body,
                    origin_to_str(&notification.origin),
                    notification.arc_id,
                    task_id_str,
                    notification.requires_response as i32,
                    is_read as i32,
                    notification.created_at.to_rfc3339(),
                    now,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Save notification: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load a notification by ID. Returns (Notification, is_read).
    pub async fn load(&self, id: Uuid) -> Result<Option<(Notification, bool)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let id_str = id.to_string();
            let mut stmt = conn
                .prepare(
                    "SELECT id, urgency, title, body, origin, arc_id, task_id, \
                     requires_response, is_read, created_at \
                     FROM notifications WHERE id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load notification: {e}")))?;

            let mut rows = stmt
                .query_map(params![id_str], row_to_notification_with_read)
                .map_err(|e| AthenError::Other(format!("Query load notification: {e}")))?;

            match rows.next() {
                Some(Ok(result)) => Ok(Some(result)),
                Some(Err(e)) => Err(AthenError::Other(format!("Read notification row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all notifications, newest first.
    pub async fn list_all(&self) -> Result<Vec<(Notification, bool)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, urgency, title, body, origin, arc_id, task_id, \
                     requires_response, is_read, created_at \
                     FROM notifications ORDER BY created_at DESC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list notifications: {e}")))?;

            let rows = stmt
                .query_map([], row_to_notification_with_read)
                .map_err(|e| AthenError::Other(format!("Query list notifications: {e}")))?;

            let mut results = Vec::new();
            for row in rows {
                results.push(
                    row.map_err(|e| AthenError::Other(format!("Read notification row: {e}")))?,
                );
            }
            Ok(results)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List only unread notifications, newest first.
    pub async fn list_unread(&self) -> Result<Vec<Notification>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, urgency, title, body, origin, arc_id, task_id, \
                     requires_response, is_read, created_at \
                     FROM notifications WHERE is_read = 0 ORDER BY created_at DESC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list unread: {e}")))?;

            let rows = stmt
                .query_map([], row_to_notification_with_read)
                .map_err(|e| AthenError::Other(format!("Query list unread: {e}")))?;

            let mut results = Vec::new();
            for row in rows {
                let (notif, _) =
                    row.map_err(|e| AthenError::Other(format!("Read notification row: {e}")))?;
                results.push(notif);
            }
            Ok(results)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Mark a single notification as read.
    pub async fn mark_read(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE notifications SET is_read = 1, updated_at = ?1 WHERE id = ?2",
                params![now, id.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Mark notification read: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Mark all notifications as read.
    pub async fn mark_all_read(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE notifications SET is_read = 1, updated_at = ?1 WHERE is_read = 0",
                params![now],
            )
            .map_err(|e| AthenError::Other(format!("Mark all notifications read: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Mark all notifications for a given arc as read.
    pub async fn mark_arc_read(&self, arc_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE notifications SET is_read = 1, updated_at = ?1 WHERE arc_id = ?2 AND is_read = 0",
                params![now, arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Mark arc notifications read: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete a notification by ID.
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM notifications WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Delete notification: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete all read notifications. Returns the number of deleted rows.
    pub async fn delete_read(&self) -> Result<u64> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count = conn
                .execute("DELETE FROM notifications WHERE is_read = 1", [])
                .map_err(|e| AthenError::Other(format!("Delete read notifications: {e}")))?;
            Ok(count as u64)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Count unread notifications.
    pub async fn unread_count(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM notifications WHERE is_read = 0",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| AthenError::Other(format!("Count unread notifications: {e}")))?;
            Ok(count as usize)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

fn urgency_to_str(urgency: &NotificationUrgency) -> &'static str {
    match urgency {
        NotificationUrgency::Low => "Low",
        NotificationUrgency::Medium => "Medium",
        NotificationUrgency::High => "High",
        NotificationUrgency::Critical => "Critical",
    }
}

fn urgency_from_str(s: &str) -> NotificationUrgency {
    match s {
        "Medium" => NotificationUrgency::Medium,
        "High" => NotificationUrgency::High,
        "Critical" => NotificationUrgency::Critical,
        _ => NotificationUrgency::Low,
    }
}

fn origin_to_str(origin: &NotificationOrigin) -> &'static str {
    match origin {
        NotificationOrigin::RiskSystem => "RiskSystem",
        NotificationOrigin::SenseRouter => "SenseRouter",
        NotificationOrigin::Agent => "Agent",
        NotificationOrigin::System => "System",
    }
}

fn origin_from_str(s: &str) -> NotificationOrigin {
    match s {
        "RiskSystem" => NotificationOrigin::RiskSystem,
        "SenseRouter" => NotificationOrigin::SenseRouter,
        "Agent" => NotificationOrigin::Agent,
        _ => NotificationOrigin::System,
    }
}

/// Map a rusqlite row to (Notification, is_read).
fn row_to_notification_with_read(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(Notification, bool)> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let urgency_str: String = row.get(1)?;
    let origin_str: String = row.get(4)?;
    let arc_id: Option<String> = row.get(5)?;
    let task_id_str: Option<String> = row.get(6)?;
    let requires_response_int: i32 = row.get(7)?;
    let is_read_int: i32 = row.get(8)?;
    let created_at_str: String = row.get(9)?;

    let task_id = task_id_str.and_then(|s| Uuid::parse_str(&s).ok());
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    Ok((
        Notification {
            id,
            urgency: urgency_from_str(&urgency_str),
            title: row.get(2)?,
            body: row.get(3)?,
            origin: origin_from_str(&origin_str),
            arc_id,
            task_id,
            created_at,
            requires_response: requires_response_int != 0,
        },
        is_read_int != 0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    async fn setup() -> NotificationStore {
        let db = Database::in_memory().await.unwrap();
        db.notification_store()
    }

    fn make_notification(title: &str, urgency: NotificationUrgency) -> Notification {
        Notification {
            id: Uuid::new_v4(),
            urgency,
            title: title.to_string(),
            body: format!("Body of {title}"),
            origin: NotificationOrigin::System,
            arc_id: None,
            task_id: None,
            created_at: Utc::now(),
            requires_response: false,
        }
    }

    #[tokio::test]
    async fn test_save_and_load() {
        let store = setup().await;
        let notif = Notification {
            id: Uuid::new_v4(),
            urgency: NotificationUrgency::High,
            title: "Test alert".to_string(),
            body: "Something happened".to_string(),
            origin: NotificationOrigin::RiskSystem,
            arc_id: Some("arc-123".to_string()),
            task_id: Some(Uuid::new_v4()),
            created_at: Utc::now(),
            requires_response: true,
        };
        let id = notif.id;

        store.save(&notif, false).await.unwrap();
        let loaded = store.load(id).await.unwrap();
        assert!(loaded.is_some());
        let (loaded_notif, is_read) = loaded.unwrap();
        assert!(!is_read);
        assert_eq!(loaded_notif.title, "Test alert");
        assert_eq!(loaded_notif.body, "Something happened");
        assert_eq!(loaded_notif.urgency, NotificationUrgency::High);
        assert_eq!(loaded_notif.origin, NotificationOrigin::RiskSystem);
        assert_eq!(loaded_notif.arc_id, Some("arc-123".to_string()));
        assert!(loaded_notif.task_id.is_some());
        assert!(loaded_notif.requires_response);
    }

    #[tokio::test]
    async fn test_list_all_ordered() {
        let store = setup().await;

        // Create 3 notifications with different timestamps.
        let mut n1 = make_notification("First", NotificationUrgency::Low);
        n1.created_at = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut n2 = make_notification("Second", NotificationUrgency::Medium);
        n2.created_at = DateTime::parse_from_rfc3339("2025-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut n3 = make_notification("Third", NotificationUrgency::High);
        n3.created_at = DateTime::parse_from_rfc3339("2025-01-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        store.save(&n1, false).await.unwrap();
        store.save(&n2, false).await.unwrap();
        store.save(&n3, false).await.unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 3);
        // Newest first.
        assert_eq!(all[0].0.title, "Third");
        assert_eq!(all[1].0.title, "Second");
        assert_eq!(all[2].0.title, "First");
    }

    #[tokio::test]
    async fn test_mark_read() {
        let store = setup().await;
        let notif = make_notification("Unread", NotificationUrgency::Medium);
        let id = notif.id;
        store.save(&notif, false).await.unwrap();

        // Verify unread.
        let (_, is_read) = store.load(id).await.unwrap().unwrap();
        assert!(!is_read);

        // Mark read.
        store.mark_read(id).await.unwrap();
        let (_, is_read) = store.load(id).await.unwrap().unwrap();
        assert!(is_read);
    }

    #[tokio::test]
    async fn test_mark_all_read() {
        let store = setup().await;
        let n1 = make_notification("A", NotificationUrgency::Low);
        let n2 = make_notification("B", NotificationUrgency::Medium);
        let n3 = make_notification("C", NotificationUrgency::High);

        store.save(&n1, false).await.unwrap();
        store.save(&n2, false).await.unwrap();
        store.save(&n3, false).await.unwrap();

        assert_eq!(store.unread_count().await.unwrap(), 3);

        store.mark_all_read().await.unwrap();
        assert_eq!(store.unread_count().await.unwrap(), 0);

        let all = store.list_all().await.unwrap();
        for (_, is_read) in &all {
            assert!(is_read);
        }
    }

    #[tokio::test]
    async fn test_mark_arc_read() {
        let store = setup().await;
        let mut n1 = make_notification("Arc A 1", NotificationUrgency::Low);
        n1.arc_id = Some("a".to_string());
        let mut n2 = make_notification("Arc A 2", NotificationUrgency::Low);
        n2.arc_id = Some("a".to_string());
        let mut n3 = make_notification("Arc B", NotificationUrgency::Low);
        n3.arc_id = Some("b".to_string());

        store.save(&n1, false).await.unwrap();
        store.save(&n2, false).await.unwrap();
        store.save(&n3, false).await.unwrap();

        store.mark_arc_read("a").await.unwrap();

        // Arc A notifications should be read.
        let (_, is_read) = store.load(n1.id).await.unwrap().unwrap();
        assert!(is_read);
        let (_, is_read) = store.load(n2.id).await.unwrap().unwrap();
        assert!(is_read);
        // Arc B should still be unread.
        let (_, is_read) = store.load(n3.id).await.unwrap().unwrap();
        assert!(!is_read);
    }

    #[tokio::test]
    async fn test_delete() {
        let store = setup().await;
        let notif = make_notification("To delete", NotificationUrgency::Low);
        let id = notif.id;
        store.save(&notif, false).await.unwrap();

        assert!(store.load(id).await.unwrap().is_some());
        store.delete(id).await.unwrap();
        assert!(store.load(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_read() {
        let store = setup().await;
        let n1 = make_notification("Read 1", NotificationUrgency::Low);
        let n2 = make_notification("Read 2", NotificationUrgency::Low);
        let n3 = make_notification("Unread", NotificationUrgency::Low);
        let unread_id = n3.id;

        store.save(&n1, true).await.unwrap();
        store.save(&n2, true).await.unwrap();
        store.save(&n3, false).await.unwrap();

        let deleted = store.delete_read().await.unwrap();
        assert_eq!(deleted, 2);

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0.id, unread_id);
    }

    #[tokio::test]
    async fn test_unread_count() {
        let store = setup().await;
        let n1 = make_notification("Read", NotificationUrgency::Low);
        let n2 = make_notification("Unread 1", NotificationUrgency::Medium);
        let n3 = make_notification("Unread 2", NotificationUrgency::High);

        store.save(&n1, true).await.unwrap();
        store.save(&n2, false).await.unwrap();
        store.save(&n3, false).await.unwrap();

        assert_eq!(store.unread_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_list_unread() {
        let store = setup().await;
        let n1 = make_notification("Read", NotificationUrgency::Low);
        let n2 = make_notification("Unread 1", NotificationUrgency::Medium);
        let n3 = make_notification("Unread 2", NotificationUrgency::High);

        store.save(&n1, true).await.unwrap();
        store.save(&n2, false).await.unwrap();
        store.save(&n3, false).await.unwrap();

        let unread = store.list_unread().await.unwrap();
        assert_eq!(unread.len(), 2);
        // All returned should be the unread ones.
        let titles: Vec<&str> = unread.iter().map(|n| n.title.as_str()).collect();
        assert!(titles.contains(&"Unread 1"));
        assert!(titles.contains(&"Unread 2"));
        assert!(!titles.contains(&"Read"));
    }
}
