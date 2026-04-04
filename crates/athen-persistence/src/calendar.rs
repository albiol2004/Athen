//! Calendar event persistence for Athen's native calendar system.
//!
//! Events are stored in SQLite alongside arc/task data. Supports recurrence,
//! reminders with fire-tracking, categories, and optional arc linking.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};

/// Who created the event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventCreator {
    User,
    Agent,
}

impl EventCreator {
    pub fn as_str(&self) -> &str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "agent" => Self::Agent,
            _ => Self::User,
        }
    }
}

/// Recurrence pattern for repeating events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Recurrence {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A calendar event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub location: Option<String>,
    pub recurrence: Option<Recurrence>,
    pub reminder_minutes: Vec<i64>,
    pub color: Option<String>,
    pub category: Option<String>,
    pub created_by: EventCreator,
    pub arc_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Tracks which reminders have already been fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiredReminder {
    pub event_id: String,
    pub reminder_minutes: i64,
    pub fired_at: String,
}

const CALENDAR_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS calendar_events (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    start_time TEXT NOT NULL,
    end_time TEXT NOT NULL,
    all_day INTEGER NOT NULL DEFAULT 0,
    location TEXT,
    recurrence TEXT,
    reminder_minutes TEXT NOT NULL DEFAULT '[]',
    color TEXT,
    category TEXT,
    created_by TEXT NOT NULL DEFAULT 'user',
    arc_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fired_reminders (
    event_id TEXT NOT NULL,
    reminder_minutes INTEGER NOT NULL,
    fired_at TEXT NOT NULL,
    PRIMARY KEY (event_id, reminder_minutes),
    FOREIGN KEY (event_id) REFERENCES calendar_events(id)
);
";

/// SQLite-backed calendar event storage.
#[derive(Clone)]
pub struct CalendarStore {
    conn: Arc<Mutex<Connection>>,
}

impl CalendarStore {
    /// Create a new `CalendarStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the calendar_events and fired_reminders tables if they do not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(CALENDAR_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init calendar schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Insert a new calendar event.
    pub async fn create_event(&self, event: &CalendarEvent) -> Result<()> {
        let conn = self.conn.clone();
        let event = event.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let reminders_json = serde_json::to_string(&event.reminder_minutes)
                .map_err(|e| AthenError::Other(format!("Serialize reminders: {e}")))?;
            let recurrence_json = event
                .recurrence
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default());
            conn.execute(
                "INSERT INTO calendar_events \
                 (id, title, description, start_time, end_time, all_day, location, \
                  recurrence, reminder_minutes, color, category, created_by, arc_id, \
                  created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    event.id,
                    event.title,
                    event.description,
                    event.start_time,
                    event.end_time,
                    event.all_day as i32,
                    event.location,
                    recurrence_json,
                    reminders_json,
                    event.color,
                    event.category,
                    event.created_by.as_str(),
                    event.arc_id,
                    event.created_at,
                    event.updated_at,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Create event: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update an existing calendar event by id, also setting updated_at.
    pub async fn update_event(&self, event: &CalendarEvent) -> Result<()> {
        let conn = self.conn.clone();
        let event = event.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let reminders_json = serde_json::to_string(&event.reminder_minutes)
                .map_err(|e| AthenError::Other(format!("Serialize reminders: {e}")))?;
            let recurrence_json = event
                .recurrence
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default());
            let now = Utc::now().to_rfc3339();
            let changed = conn
                .execute(
                    "UPDATE calendar_events SET \
                     title = ?1, description = ?2, start_time = ?3, end_time = ?4, \
                     all_day = ?5, location = ?6, recurrence = ?7, reminder_minutes = ?8, \
                     color = ?9, category = ?10, created_by = ?11, arc_id = ?12, \
                     updated_at = ?13 \
                     WHERE id = ?14",
                    params![
                        event.title,
                        event.description,
                        event.start_time,
                        event.end_time,
                        event.all_day as i32,
                        event.location,
                        recurrence_json,
                        reminders_json,
                        event.color,
                        event.category,
                        event.created_by.as_str(),
                        event.arc_id,
                        now,
                        event.id,
                    ],
                )
                .map_err(|e| AthenError::Other(format!("Update event: {e}")))?;
            if changed == 0 {
                return Err(AthenError::Other(format!(
                    "Event not found: {}",
                    event.id
                )));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete a calendar event and its fired reminders.
    pub async fn delete_event(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM fired_reminders WHERE event_id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete fired reminders: {e}")))?;
            conn.execute("DELETE FROM calendar_events WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete event: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Retrieve a single event by id.
    pub async fn get_event(&self, id: &str) -> Result<Option<CalendarEvent>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, start_time, end_time, all_day, \
                     location, recurrence, reminder_minutes, color, category, \
                     created_by, arc_id, created_at, updated_at \
                     FROM calendar_events WHERE id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare get event: {e}")))?;

            let mut rows = stmt
                .query_map(params![id], row_to_event)
                .map_err(|e| AthenError::Other(format!("Query get event: {e}")))?;

            match rows.next() {
                Some(Ok(event)) => Ok(Some(event)),
                Some(Err(e)) => Err(AthenError::Other(format!("Read event row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List events whose time range overlaps [start, end], ordered by start_time.
    ///
    /// An event overlaps if its start_time < end AND its end_time > start.
    pub async fn list_events(&self, start: &str, end: &str) -> Result<Vec<CalendarEvent>> {
        let conn = self.conn.clone();
        let start = start.to_string();
        let end = end.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, start_time, end_time, all_day, \
                     location, recurrence, reminder_minutes, color, category, \
                     created_by, arc_id, created_at, updated_at \
                     FROM calendar_events \
                     WHERE datetime(start_time) < datetime(?2) AND datetime(end_time) > datetime(?1) \
                     ORDER BY start_time ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list events: {e}")))?;

            let rows = stmt
                .query_map(params![start, end], row_to_event)
                .map_err(|e| AthenError::Other(format!("Query list events: {e}")))?;

            let mut events = Vec::new();
            for row in rows {
                events.push(
                    row.map_err(|e| AthenError::Other(format!("Read event row: {e}")))?,
                );
            }
            Ok(events)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all events ordered by start_time.
    pub async fn list_all_events(&self) -> Result<Vec<CalendarEvent>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, start_time, end_time, all_day, \
                     location, recurrence, reminder_minutes, color, category, \
                     created_by, arc_id, created_at, updated_at \
                     FROM calendar_events ORDER BY start_time ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list all events: {e}")))?;

            let rows = stmt
                .query_map([], row_to_event)
                .map_err(|e| AthenError::Other(format!("Query list all events: {e}")))?;

            let mut events = Vec::new();
            for row in rows {
                events.push(
                    row.map_err(|e| AthenError::Other(format!("Read event row: {e}")))?,
                );
            }
            Ok(events)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Get events starting within the next N minutes from now.
    pub async fn get_upcoming_events(&self, within_minutes: i64) -> Result<Vec<CalendarEvent>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            let future = (Utc::now() + chrono::Duration::minutes(within_minutes)).to_rfc3339();

            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, start_time, end_time, all_day, \
                     location, recurrence, reminder_minutes, color, category, \
                     created_by, arc_id, created_at, updated_at \
                     FROM calendar_events \
                     WHERE datetime(start_time) >= datetime(?1) AND datetime(start_time) <= datetime(?2) \
                     ORDER BY start_time ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare upcoming events: {e}")))?;

            let rows = stmt
                .query_map(params![now, future], row_to_event)
                .map_err(|e| AthenError::Other(format!("Query upcoming events: {e}")))?;

            let mut events = Vec::new();
            for row in rows {
                events.push(
                    row.map_err(|e| AthenError::Other(format!("Read event row: {e}")))?,
                );
            }
            Ok(events)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Get events filtered by category, ordered by start_time.
    pub async fn get_events_by_category(&self, category: &str) -> Result<Vec<CalendarEvent>> {
        let conn = self.conn.clone();
        let category = category.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, start_time, end_time, all_day, \
                     location, recurrence, reminder_minutes, color, category, \
                     created_by, arc_id, created_at, updated_at \
                     FROM calendar_events WHERE category = ?1 ORDER BY start_time ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare events by category: {e}")))?;

            let rows = stmt
                .query_map(params![category], row_to_event)
                .map_err(|e| AthenError::Other(format!("Query events by category: {e}")))?;

            let mut events = Vec::new();
            for row in rows {
                events.push(
                    row.map_err(|e| AthenError::Other(format!("Read event row: {e}")))?,
                );
            }
            Ok(events)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Record that a specific reminder for an event has been fired.
    pub async fn record_fired_reminder(
        &self,
        event_id: &str,
        reminder_minutes: i64,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let event_id = event_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT OR REPLACE INTO fired_reminders (event_id, reminder_minutes, fired_at) \
                 VALUES (?1, ?2, ?3)",
                params![event_id, reminder_minutes, now],
            )
            .map_err(|e| AthenError::Other(format!("Record fired reminder: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Check if a specific reminder has already been fired.
    pub async fn is_reminder_fired(&self, event_id: &str, reminder_minutes: i64) -> Result<bool> {
        let conn = self.conn.clone();
        let event_id = event_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM fired_reminders \
                     WHERE event_id = ?1 AND reminder_minutes = ?2",
                    params![event_id, reminder_minutes],
                    |row| row.get(0),
                )
                .map_err(|e| AthenError::Other(format!("Check fired reminder: {e}")))?;
            Ok(count > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete fired reminders older than the given timestamp.
    pub async fn clear_old_fired_reminders(&self, before: &str) -> Result<()> {
        let conn = self.conn.clone();
        let before = before.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM fired_reminders WHERE fired_at < ?1",
                params![before],
            )
            .map_err(|e| AthenError::Other(format!("Clear old fired reminders: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

/// Map a rusqlite row to a CalendarEvent.
fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let recurrence_str: Option<String> = row.get(7)?;
    let recurrence = recurrence_str.and_then(|s| serde_json::from_str(&s).ok());

    let reminders_str: String = row.get(8)?;
    let reminder_minutes: Vec<i64> =
        serde_json::from_str(&reminders_str).unwrap_or_default();

    let all_day_int: i32 = row.get(5)?;

    Ok(CalendarEvent {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        start_time: row.get(3)?,
        end_time: row.get(4)?,
        all_day: all_day_int != 0,
        location: row.get(6)?,
        recurrence,
        reminder_minutes,
        color: row.get(9)?,
        category: row.get(10)?,
        created_by: EventCreator::from_str(&row.get::<_, String>(11)?),
        arc_id: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> CalendarStore {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = CalendarStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    fn make_event(id: &str, title: &str, start: &str, end: &str) -> CalendarEvent {
        let now = Utc::now().to_rfc3339();
        CalendarEvent {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            start_time: start.to_string(),
            end_time: end.to_string(),
            all_day: false,
            location: None,
            recurrence: None,
            reminder_minutes: vec![],
            color: None,
            category: None,
            created_by: EventCreator::User,
            arc_id: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn test_create_and_get_event() {
        let store = setup().await;
        let event = make_event("e1", "Standup", "2026-04-04T09:00:00Z", "2026-04-04T09:30:00Z");
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("e1").await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "e1");
        assert_eq!(loaded.title, "Standup");
    }

    #[tokio::test]
    async fn test_get_nonexistent_event() {
        let store = setup().await;
        let loaded = store.get_event("nope").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_update_event() {
        let store = setup().await;
        let mut event = make_event("e2", "Draft", "2026-04-05T10:00:00Z", "2026-04-05T11:00:00Z");
        store.create_event(&event).await.unwrap();

        event.title = "Final Meeting".to_string();
        event.description = Some("Important".to_string());
        store.update_event(&event).await.unwrap();

        let loaded = store.get_event("e2").await.unwrap().unwrap();
        assert_eq!(loaded.title, "Final Meeting");
        assert_eq!(loaded.description, Some("Important".to_string()));
    }

    #[tokio::test]
    async fn test_update_sets_updated_at() {
        let store = setup().await;
        let event = make_event("e_upd", "Old", "2026-04-05T10:00:00Z", "2026-04-05T11:00:00Z");
        store.create_event(&event).await.unwrap();
        let original = store.get_event("e_upd").await.unwrap().unwrap();

        // Small delay to ensure different timestamp
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mut updated = event.clone();
        updated.title = "New".to_string();
        store.update_event(&updated).await.unwrap();

        let reloaded = store.get_event("e_upd").await.unwrap().unwrap();
        assert_ne!(reloaded.updated_at, original.updated_at);
    }

    #[tokio::test]
    async fn test_update_nonexistent_event() {
        let store = setup().await;
        let event = make_event("ghost", "Nope", "2026-04-05T10:00:00Z", "2026-04-05T11:00:00Z");
        let result = store.update_event(&event).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_event() {
        let store = setup().await;
        let event = make_event("e3", "Delete me", "2026-04-05T10:00:00Z", "2026-04-05T11:00:00Z");
        store.create_event(&event).await.unwrap();
        store.delete_event("e3").await.unwrap();

        let loaded = store.get_event("e3").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_delete_also_clears_fired_reminders() {
        let store = setup().await;
        let mut event = make_event("e_del", "Bye", "2026-04-05T10:00:00Z", "2026-04-05T11:00:00Z");
        event.reminder_minutes = vec![15];
        store.create_event(&event).await.unwrap();
        store.record_fired_reminder("e_del", 15).await.unwrap();
        assert!(store.is_reminder_fired("e_del", 15).await.unwrap());

        store.delete_event("e_del").await.unwrap();
        // Fired reminders should be gone too
        assert!(!store.is_reminder_fired("e_del", 15).await.unwrap());
    }

    #[tokio::test]
    async fn test_list_events_by_time_range() {
        let store = setup().await;
        // Event A: 09:00-10:00
        let a = make_event("a", "A", "2026-04-04T09:00:00Z", "2026-04-04T10:00:00Z");
        // Event B: 10:00-11:00
        let b = make_event("b", "B", "2026-04-04T10:00:00Z", "2026-04-04T11:00:00Z");
        // Event C: 14:00-15:00 (outside range)
        let c = make_event("c", "C", "2026-04-04T14:00:00Z", "2026-04-04T15:00:00Z");

        store.create_event(&a).await.unwrap();
        store.create_event(&b).await.unwrap();
        store.create_event(&c).await.unwrap();

        // Query for 09:30-10:30 should overlap A (ends after 09:30) and B (starts before 10:30)
        let results = store
            .list_events("2026-04-04T09:30:00Z", "2026-04-04T10:30:00Z")
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert_eq!(results[1].id, "b");
    }

    #[tokio::test]
    async fn test_list_events_empty_range() {
        let store = setup().await;
        let event = make_event("e", "E", "2026-04-04T09:00:00Z", "2026-04-04T10:00:00Z");
        store.create_event(&event).await.unwrap();

        let results = store
            .list_events("2026-04-05T00:00:00Z", "2026-04-05T23:59:59Z")
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_list_all_events() {
        let store = setup().await;
        let a = make_event("a1", "First", "2026-04-04T08:00:00Z", "2026-04-04T09:00:00Z");
        let b = make_event("b1", "Second", "2026-04-04T10:00:00Z", "2026-04-04T11:00:00Z");
        store.create_event(&a).await.unwrap();
        store.create_event(&b).await.unwrap();

        let all = store.list_all_events().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title, "First");
        assert_eq!(all[1].title, "Second");
    }

    #[tokio::test]
    async fn test_get_events_by_category() {
        let store = setup().await;
        let mut meeting = make_event("m1", "Standup", "2026-04-04T09:00:00Z", "2026-04-04T09:30:00Z");
        meeting.category = Some("meeting".to_string());
        let mut birthday = make_event("b1", "Alex bday", "2026-04-04T00:00:00Z", "2026-04-05T00:00:00Z");
        birthday.category = Some("birthday".to_string());

        store.create_event(&meeting).await.unwrap();
        store.create_event(&birthday).await.unwrap();

        let meetings = store.get_events_by_category("meeting").await.unwrap();
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].id, "m1");

        let empty = store.get_events_by_category("deadline").await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_all_day_event() {
        let store = setup().await;
        let mut event = make_event("ad1", "Holiday", "2026-12-25T00:00:00Z", "2026-12-26T00:00:00Z");
        event.all_day = true;
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("ad1").await.unwrap().unwrap();
        assert!(loaded.all_day);
    }

    #[tokio::test]
    async fn test_recurrence_round_trip() {
        let store = setup().await;
        for recurrence in [Recurrence::Daily, Recurrence::Weekly, Recurrence::Monthly, Recurrence::Yearly] {
            let mut event = make_event(
                &format!("rec_{:?}", recurrence),
                "Recurring",
                "2026-04-04T09:00:00Z",
                "2026-04-04T10:00:00Z",
            );
            event.recurrence = Some(recurrence.clone());
            store.create_event(&event).await.unwrap();

            let loaded = store.get_event(&event.id).await.unwrap().unwrap();
            assert_eq!(loaded.recurrence, Some(recurrence));
        }
    }

    #[tokio::test]
    async fn test_reminder_firing_tracking() {
        let store = setup().await;
        let mut event = make_event("rem1", "Remind me", "2026-04-04T14:00:00Z", "2026-04-04T15:00:00Z");
        event.reminder_minutes = vec![15, 60];
        store.create_event(&event).await.unwrap();

        // Not fired yet
        assert!(!store.is_reminder_fired("rem1", 15).await.unwrap());
        assert!(!store.is_reminder_fired("rem1", 60).await.unwrap());

        // Fire the 15-minute reminder
        store.record_fired_reminder("rem1", 15).await.unwrap();
        assert!(store.is_reminder_fired("rem1", 15).await.unwrap());
        assert!(!store.is_reminder_fired("rem1", 60).await.unwrap());

        // Fire the 60-minute reminder
        store.record_fired_reminder("rem1", 60).await.unwrap();
        assert!(store.is_reminder_fired("rem1", 60).await.unwrap());
    }

    #[tokio::test]
    async fn test_clear_old_fired_reminders() {
        let store = setup().await;
        let event = make_event("old1", "Old", "2026-01-01T09:00:00Z", "2026-01-01T10:00:00Z");
        store.create_event(&event).await.unwrap();
        store.record_fired_reminder("old1", 15).await.unwrap();
        assert!(store.is_reminder_fired("old1", 15).await.unwrap());

        // Clear reminders fired before a future date (should clear everything)
        store
            .clear_old_fired_reminders("2099-01-01T00:00:00Z")
            .await
            .unwrap();
        assert!(!store.is_reminder_fired("old1", 15).await.unwrap());
    }

    #[tokio::test]
    async fn test_event_with_no_reminders() {
        let store = setup().await;
        let event = make_event("nr1", "No reminders", "2026-04-04T09:00:00Z", "2026-04-04T10:00:00Z");
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("nr1").await.unwrap().unwrap();
        assert!(loaded.reminder_minutes.is_empty());
    }

    #[tokio::test]
    async fn test_event_with_arc_link() {
        let store = setup().await;
        let mut event = make_event("arc1", "Linked", "2026-04-04T09:00:00Z", "2026-04-04T10:00:00Z");
        event.arc_id = Some("arc-uuid-123".to_string());
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("arc1").await.unwrap().unwrap();
        assert_eq!(loaded.arc_id, Some("arc-uuid-123".to_string()));
    }

    #[tokio::test]
    async fn test_agent_created_event() {
        let store = setup().await;
        let mut event = make_event("ag1", "Agent task", "2026-04-04T09:00:00Z", "2026-04-04T10:00:00Z");
        event.created_by = EventCreator::Agent;
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("ag1").await.unwrap().unwrap();
        assert_eq!(loaded.created_by, EventCreator::Agent);
    }

    #[tokio::test]
    async fn test_event_with_all_fields() {
        let store = setup().await;
        let now = Utc::now().to_rfc3339();
        let event = CalendarEvent {
            id: "full1".to_string(),
            title: "Full event".to_string(),
            description: Some("A complete event".to_string()),
            start_time: "2026-04-04T14:00:00Z".to_string(),
            end_time: "2026-04-04T15:30:00Z".to_string(),
            all_day: false,
            location: Some("Conference Room B".to_string()),
            recurrence: Some(Recurrence::Weekly),
            reminder_minutes: vec![5, 15, 60],
            color: Some("#ff5733".to_string()),
            category: Some("meeting".to_string()),
            created_by: EventCreator::User,
            arc_id: Some("arc-999".to_string()),
            created_at: now.clone(),
            updated_at: now,
        };
        store.create_event(&event).await.unwrap();

        let loaded = store.get_event("full1").await.unwrap().unwrap();
        assert_eq!(loaded.title, "Full event");
        assert_eq!(loaded.description, Some("A complete event".to_string()));
        assert_eq!(loaded.location, Some("Conference Room B".to_string()));
        assert_eq!(loaded.recurrence, Some(Recurrence::Weekly));
        assert_eq!(loaded.reminder_minutes, vec![5, 15, 60]);
        assert_eq!(loaded.color, Some("#ff5733".to_string()));
        assert_eq!(loaded.category, Some("meeting".to_string()));
        assert_eq!(loaded.arc_id, Some("arc-999".to_string()));
        assert!(!loaded.all_day);
    }
}
