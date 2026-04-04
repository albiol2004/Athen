//! Calendar sense monitor.
//!
//! Polls the local SQLite calendar database for upcoming events and fires
//! reminders as [`SenseEvent`]s. Tracks which reminders have already been
//! sent to avoid duplicate notifications within a session.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use athen_core::config::AthenConfig;
use athen_core::error::{AthenError, Result};
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// A calendar event row from the local SQLite database.
#[derive(Debug, Clone)]
pub struct UpcomingEvent {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub location: Option<String>,
    pub category: Option<String>,
    /// Reminder lead times in minutes, e.g. `[15, 60]`.
    pub reminder_minutes: Vec<i64>,
}

/// Calendar sense monitor.
///
/// Polls the local calendar database for upcoming events and fires
/// reminders as SenseEvents. Tracks which reminders have been sent
/// to avoid duplicates.
pub struct CalendarMonitor {
    poll_interval: Duration,
    /// Path to the SQLite database file. Set during `init()`.
    db_path: Option<String>,
    /// Track `(event_id, reminder_minutes)` pairs we have already fired
    /// to avoid duplicate notifications within a session.
    fired_reminders: Mutex<HashSet<(String, i64)>>,
}

impl CalendarMonitor {
    /// Create a new `CalendarMonitor` with the default poll interval of 60 seconds.
    pub fn new() -> Self {
        Self {
            poll_interval: Duration::from_secs(60),
            db_path: None,
            fired_reminders: Mutex::new(HashSet::new()),
        }
    }

    /// Create a `CalendarMonitor` with a custom poll interval.
    pub fn with_interval(poll_interval: Duration) -> Self {
        Self {
            poll_interval,
            db_path: None,
            fired_reminders: Mutex::new(HashSet::new()),
        }
    }

    /// Create a `CalendarMonitor` with a pre-set database path (useful for testing).
    pub fn with_db_path(db_path: String) -> Self {
        Self {
            poll_interval: Duration::from_secs(60),
            db_path: Some(db_path),
            fired_reminders: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for CalendarMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the default database path: `~/.athen/athen.db`.
fn default_db_path() -> Option<String> {
    dirs_path().map(|mut p| {
        p.push("athen.db");
        p.to_string_lossy().to_string()
    })
}

/// Return `~/.athen/` as a `PathBuf`, or `None` if the home directory cannot be determined.
fn dirs_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".athen"))
}

/// Cross-platform home directory lookup.
fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE").ok().map(std::path::PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME").ok().map(std::path::PathBuf::from)
    }
}

/// Open the calendar database and query for events that might need a
/// reminder right now.
///
/// Instead of a fixed lookahead, we query all future events within the next
/// 7 days and let `generate_reminder_events` decide which reminders to fire
/// based on each event's `reminder_minutes`. This correctly handles long-lead
/// reminders like "1 day before".
fn query_upcoming_events(
    db_path: &str,
    now: DateTime<Utc>,
) -> Result<Vec<UpcomingEvent>> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| AthenError::Other(format!("Calendar DB open failed: {e}")))?;

    // Check that the calendar_events table exists. If not, return empty —
    // the table may not have been created yet.
    let table_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='calendar_events'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !table_exists {
        return Ok(Vec::new());
    }

    // Query future events within 7 days. We use datetime() for proper
    // comparison that works regardless of timezone suffix format
    // (both `+00:00` and `Z` are handled correctly by SQLite's datetime()).
    let until = now + chrono::Duration::days(7);

    let mut stmt = conn
        .prepare(
            "SELECT id, title, description, start_time, end_time, \
             location, category, reminder_minutes \
             FROM calendar_events \
             WHERE datetime(start_time) >= datetime(?1) \
               AND datetime(start_time) <= datetime(?2)",
        )
        .map_err(|e| AthenError::Other(format!("Calendar query prepare: {e}")))?;

    let now_str = now.to_rfc3339();
    let until_str = until.to_rfc3339();

    let rows = stmt
        .query_map(rusqlite::params![now_str, until_str], |row| {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let description: Option<String> = row.get(2)?;
            let start_time_str: String = row.get(3)?;
            let end_time_str: Option<String> = row.get(4)?;
            let location: Option<String> = row.get(5)?;
            let category: Option<String> = row.get(6)?;
            let reminder_json: Option<String> = row.get(7)?;

            Ok((
                id,
                title,
                description,
                start_time_str,
                end_time_str,
                location,
                category,
                reminder_json,
            ))
        })
        .map_err(|e| AthenError::Other(format!("Calendar query execute: {e}")))?;

    let mut events = Vec::new();

    for row_result in rows {
        let (id, title, description, start_str, end_str, location, category, reminder_json) =
            match row_result {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Skipping calendar row: {e}");
                    continue;
                }
            };

        let start_time = match DateTime::parse_from_rfc3339(&start_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                tracing::warn!("Bad start_time '{start_str}' for event {id}: {e}");
                continue;
            }
        };

        let end_time = end_str.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        });

        let reminder_minutes = parse_reminder_minutes(reminder_json.as_deref());

        events.push(UpcomingEvent {
            id,
            title,
            description,
            start_time,
            end_time,
            location,
            category,
            reminder_minutes,
        });
    }

    Ok(events)
}

/// Parse the `reminder_minutes` JSON column. Accepts a JSON array of integers,
/// e.g. `"[15, 60]"`, or returns an empty vec on parse failure.
fn parse_reminder_minutes(json: Option<&str>) -> Vec<i64> {
    let Some(s) = json else {
        return Vec::new();
    };
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<i64>>(s).unwrap_or_default()
}

/// Build a `SenseEvent` for a calendar reminder.
pub fn build_reminder_event(event: &UpcomingEvent, minutes_until: i64) -> SenseEvent {
    let summary = if minutes_until <= 0 {
        format!("Event starting now: {}", event.title)
    } else {
        format!("Reminder: {} in {} minutes", event.title, minutes_until)
    };

    let body = serde_json::json!({
        "event_id": event.id,
        "title": event.title,
        "start_time": event.start_time.to_rfc3339(),
        "end_time": event.end_time.map(|t| t.to_rfc3339()),
        "location": event.location,
        "description": event.description,
        "category": event.category,
        "minutes_until": minutes_until,
    });

    SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::Calendar,
        kind: EventKind::Reminder,
        sender: None,
        content: NormalizedContent {
            summary: Some(summary),
            body,
            attachments: vec![],
        },
        source_risk: RiskLevel::Safe,
        raw_id: Some(format!("{}_{}", event.id, minutes_until)),
    }
}

/// Determine which reminders should fire for the given events at the current time.
///
/// Returns the list of `SenseEvent`s and marks fired reminders in `fired_set` to
/// prevent duplicates.
pub fn generate_reminder_events(
    events: &[UpcomingEvent],
    now: DateTime<Utc>,
    fired_set: &Mutex<HashSet<(String, i64)>>,
) -> Vec<SenseEvent> {
    let mut result = Vec::new();
    let mut guard = fired_set.lock().unwrap();

    for event in events {
        let diff = event.start_time.signed_duration_since(now);
        let minutes_until = diff.num_minutes();

        // Check explicit reminders.
        for &rem in &event.reminder_minutes {
            if minutes_until <= rem {
                let key = (event.id.clone(), rem);
                if !guard.contains(&key) {
                    result.push(build_reminder_event(event, minutes_until));
                    guard.insert(key);
                }
            }
        }

        // "Starting now" notification (within 1 minute) regardless of explicit reminders.
        if (0..=1).contains(&minutes_until) {
            let key = (event.id.clone(), 0);
            if !guard.contains(&key) {
                result.push(build_reminder_event(event, 0));
                guard.insert(key);
            }
        }
    }

    result
}

#[async_trait]
impl SenseMonitor for CalendarMonitor {
    fn sense_id(&self) -> &str {
        "calendar"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        if self.db_path.is_none() {
            self.db_path = default_db_path();
        }
        tracing::info!(
            db_path = ?self.db_path,
            "CalendarMonitor initialized"
        );
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        let db_path = match &self.db_path {
            Some(p) => p.clone(),
            None => {
                tracing::debug!("CalendarMonitor: no database path configured");
                return Ok(Vec::new());
            }
        };

        // Check that the database file exists at all.
        if !std::path::Path::new(&db_path).exists() {
            tracing::debug!("CalendarMonitor: database file does not exist yet");
            return Ok(Vec::new());
        }

        let now = Utc::now();

        let events = {
            let path = db_path.clone();
            tokio::task::spawn_blocking(move || query_upcoming_events(&path, now))
                .await
                .map_err(|e| AthenError::Other(format!("Calendar poll task panicked: {e}")))?
        }?;

        let sense_events = generate_reminder_events(&events, now, &self.fired_reminders);

        if !sense_events.is_empty() {
            tracing::info!(
                count = sense_events.len(),
                "CalendarMonitor: generated reminder events"
            );
        }

        Ok(sense_events)
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("CalendarMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Basic monitor properties
    // ---------------------------------------------------------------

    #[test]
    fn sense_id_is_calendar() {
        let monitor = CalendarMonitor::new();
        assert_eq!(monitor.sense_id(), "calendar");
    }

    #[test]
    fn default_poll_interval_is_60s() {
        let monitor = CalendarMonitor::new();
        assert_eq!(monitor.poll_interval(), Duration::from_secs(60));
    }

    #[test]
    fn custom_interval_works() {
        let monitor = CalendarMonitor::with_interval(Duration::from_secs(30));
        assert_eq!(monitor.poll_interval(), Duration::from_secs(30));
    }

    // ---------------------------------------------------------------
    // Graceful handling when no DB is configured
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn poll_with_no_db_path_returns_empty() {
        let monitor = CalendarMonitor::new();
        // db_path is None — should return empty without error.
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_with_nonexistent_db_returns_empty() {
        let monitor = CalendarMonitor::with_db_path("/tmp/athen_test_nonexistent_12345.db".into());
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    // ---------------------------------------------------------------
    // Helper: parse_reminder_minutes
    // ---------------------------------------------------------------

    #[test]
    fn parse_reminder_minutes_valid() {
        assert_eq!(parse_reminder_minutes(Some("[15, 60]")), vec![15, 60]);
    }

    #[test]
    fn parse_reminder_minutes_none() {
        assert!(parse_reminder_minutes(None).is_empty());
    }

    #[test]
    fn parse_reminder_minutes_empty_string() {
        assert!(parse_reminder_minutes(Some("")).is_empty());
    }

    #[test]
    fn parse_reminder_minutes_invalid_json() {
        assert!(parse_reminder_minutes(Some("not json")).is_empty());
    }

    // ---------------------------------------------------------------
    // Helper: build_reminder_event
    // ---------------------------------------------------------------

    #[test]
    fn build_reminder_event_format() {
        let event = UpcomingEvent {
            id: "evt-1".into(),
            title: "Meeting with John".into(),
            description: Some("Discuss Q3 plans".into()),
            start_time: Utc::now() + chrono::Duration::minutes(15),
            end_time: Some(Utc::now() + chrono::Duration::minutes(75)),
            location: Some("Room 42".into()),
            category: Some("work".into()),
            reminder_minutes: vec![15, 60],
        };

        let se = build_reminder_event(&event, 15);

        assert_eq!(se.source, EventSource::Calendar);
        assert!(matches!(se.kind, EventKind::Reminder));
        assert_eq!(se.source_risk, RiskLevel::Safe);
        assert_eq!(
            se.content.summary.as_deref(),
            Some("Reminder: Meeting with John in 15 minutes")
        );
        assert_eq!(se.raw_id.as_deref(), Some("evt-1_15"));
        assert_eq!(se.content.body["event_id"], "evt-1");
        assert_eq!(se.content.body["title"], "Meeting with John");
        assert_eq!(se.content.body["location"], "Room 42");
        assert_eq!(se.content.body["description"], "Discuss Q3 plans");
        assert_eq!(se.content.body["category"], "work");
        assert_eq!(se.content.body["minutes_until"], 15);
        assert!(se.content.attachments.is_empty());
    }

    #[test]
    fn build_reminder_event_starting_now() {
        let event = UpcomingEvent {
            id: "evt-2".into(),
            title: "Standup".into(),
            description: None,
            start_time: Utc::now(),
            end_time: None,
            location: None,
            category: None,
            reminder_minutes: vec![],
        };

        let se = build_reminder_event(&event, 0);
        assert_eq!(
            se.content.summary.as_deref(),
            Some("Event starting now: Standup")
        );
        assert_eq!(se.raw_id.as_deref(), Some("evt-2_0"));
    }

    // ---------------------------------------------------------------
    // Dedup tracking
    // ---------------------------------------------------------------

    #[test]
    fn fired_reminder_dedup_tracking() {
        let fired = Mutex::new(HashSet::new());
        let now = Utc::now();
        let event = UpcomingEvent {
            id: "evt-3".into(),
            title: "Call".into(),
            description: None,
            start_time: now + chrono::Duration::minutes(10),
            end_time: None,
            location: None,
            category: None,
            reminder_minutes: vec![15],
        };

        // First call should generate a reminder.
        let first = generate_reminder_events(std::slice::from_ref(&event), now, &fired);
        assert_eq!(first.len(), 1);

        // Second call with same event should be deduped.
        let second = generate_reminder_events(&[event], now, &fired);
        assert!(second.is_empty());
    }

    // ---------------------------------------------------------------
    // Event starting now detection
    // ---------------------------------------------------------------

    #[test]
    fn event_starting_now_detection() {
        let fired = Mutex::new(HashSet::new());
        let now = Utc::now();
        let event = UpcomingEvent {
            id: "evt-now".into(),
            title: "Lunch".into(),
            description: None,
            start_time: now + chrono::Duration::seconds(30),
            end_time: None,
            location: None,
            category: None,
            // No explicit reminders — should still get a "starting now" event.
            reminder_minutes: vec![],
        };

        let events = generate_reminder_events(&[event], now, &fired);
        assert_eq!(events.len(), 1);
        assert!(events[0]
            .content
            .summary
            .as_ref()
            .unwrap()
            .contains("starting now"));
    }

    // ---------------------------------------------------------------
    // Multiple reminders for same event
    // ---------------------------------------------------------------

    #[test]
    fn multiple_reminders_for_same_event() {
        let fired = Mutex::new(HashSet::new());
        let now = Utc::now();
        // Event starts in 10 minutes — both the 15-min and 60-min reminders should fire.
        let event = UpcomingEvent {
            id: "evt-multi".into(),
            title: "Review".into(),
            description: None,
            start_time: now + chrono::Duration::minutes(10),
            end_time: None,
            location: None,
            category: None,
            reminder_minutes: vec![15, 60],
        };

        let events = generate_reminder_events(&[event], now, &fired);
        // 15-min reminder fires (10 <= 15), 60-min reminder fires (10 <= 60),
        // plus "starting now" does NOT fire since minutes_until == 10 > 1.
        assert_eq!(events.len(), 2);
    }

    // ---------------------------------------------------------------
    // Shutdown
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = CalendarMonitor::new();
        monitor.shutdown().await.unwrap();
    }

    // ---------------------------------------------------------------
    // Full poll with a real temp SQLite database
    // ---------------------------------------------------------------

    /// Helper: create a temp database with the calendar_events table and return its path.
    fn create_test_db() -> (tempfile::NamedTempFile, String) {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_string_lossy().to_string();

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE calendar_events (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT,
                start_time TEXT NOT NULL,
                end_time TEXT,
                all_day INTEGER DEFAULT 0,
                location TEXT,
                recurrence TEXT,
                reminder_minutes TEXT,
                color TEXT,
                category TEXT,
                created_by TEXT,
                arc_id TEXT,
                created_at TEXT,
                updated_at TEXT
            );",
        )
        .unwrap();

        (tmp, path)
    }

    #[tokio::test]
    async fn poll_with_empty_table_returns_empty() {
        let (_tmp, path) = create_test_db();
        let monitor = CalendarMonitor::with_db_path(path);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_finds_upcoming_event_with_reminder() {
        let (_tmp, path) = create_test_db();

        // Insert an event starting 10 minutes from now with a 15-minute reminder.
        let start = Utc::now() + chrono::Duration::minutes(10);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO calendar_events (id, title, start_time, reminder_minutes)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["evt-poll-1", "Team Sync", start.to_rfc3339(), "[15]"],
        )
        .unwrap();
        drop(conn);

        let monitor = CalendarMonitor::with_db_path(path);
        let events = monitor.poll().await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0]
            .content
            .summary
            .as_ref()
            .unwrap()
            .contains("Team Sync"));
    }

    #[tokio::test]
    async fn poll_does_not_fire_for_past_events() {
        let (_tmp, path) = create_test_db();

        // Insert an event that already started 5 minutes ago.
        let start = Utc::now() - chrono::Duration::minutes(5);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO calendar_events (id, title, start_time, reminder_minutes)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["evt-past", "Old Meeting", start.to_rfc3339(), "[15]"],
        )
        .unwrap();
        drop(conn);

        let monitor = CalendarMonitor::with_db_path(path);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_does_not_fire_for_far_future_events() {
        let (_tmp, path) = create_test_db();

        // Insert an event starting 3 hours from now — outside the 60-min lookahead.
        let start = Utc::now() + chrono::Duration::hours(3);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO calendar_events (id, title, start_time, reminder_minutes)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["evt-far", "Future Meeting", start.to_rfc3339(), "[15]"],
        )
        .unwrap();
        drop(conn);

        let monitor = CalendarMonitor::with_db_path(path);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_dedup_across_calls() {
        let (_tmp, path) = create_test_db();

        let start = Utc::now() + chrono::Duration::minutes(10);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO calendar_events (id, title, start_time, reminder_minutes)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["evt-dedup", "Dedup Test", start.to_rfc3339(), "[15]"],
        )
        .unwrap();
        drop(conn);

        let monitor = CalendarMonitor::with_db_path(path);

        let first = monitor.poll().await.unwrap();
        assert_eq!(first.len(), 1);

        // Second poll should not fire the same reminder again.
        let second = monitor.poll().await.unwrap();
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn poll_db_without_table_returns_empty() {
        // Create a database file without the calendar_events table.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE other_table (id TEXT);")
            .unwrap();
        drop(conn);

        let monitor = CalendarMonitor::with_db_path(path);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }
}
