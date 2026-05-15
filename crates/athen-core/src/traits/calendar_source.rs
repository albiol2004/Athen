//! Calendar source port.
//!
//! A [`CalendarSource`] is the producer side of Athen's calendar pipeline.
//! Adapters (CalDAV, Microsoft Graph, Google Calendar) implement this trait;
//! a sync loop in the composition root pulls [`RemoteEvent`]s and writes
//! them into the local SQLite [`CalendarStore`](../../../athen-persistence)
//! that the [`CalendarMonitor`](../sense) already polls.
//!
//! The trait deliberately mirrors the shape of the local
//! [`CalendarEvent`](../../../athen-persistence) struct, plus per-source
//! bookkeeping (`remote_id`, `etag`, `ical_uid`) so the sync loop can diff
//! efficiently and so events that appear in two sources (a Gmail invite
//! also synced to iCloud) can be deduped on `ical_uid`.
//!
//! Per-source config (base URL, username, calendar IDs, vault-backed
//! credential reference) lives in the persistence layer — adapter
//! constructors take only the fully-resolved values they need to make
//! HTTP calls. This keeps `athen-core` free of vault and storage concerns.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One sub-collection within a [`CalendarSource`] — what a user calls a
/// "calendar" in their provider's UI ("Home", "Work", "Family", etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCalendar {
    /// Provider-specific identifier (CalDAV: the collection URL; Graph: the
    /// calendar id). Opaque to Athen — passed back verbatim on subsequent
    /// `list_events` / `create_event` calls.
    pub id: String,
    pub name: String,
    /// Display color as a `#rrggbb` string when the provider exposes one.
    pub color: Option<String>,
    /// True when the source cannot write to this calendar (shared read-only
    /// subscription, holiday calendar, etc.).
    pub read_only: bool,
}

/// An event as it lives on a remote source. The sync loop translates this
/// into the local `CalendarEvent` shape on its way into SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEvent {
    /// Provider's primary key for the event (CalDAV: object href; Graph:
    /// event id). Stable across edits within one source.
    pub remote_id: String,
    /// Which [`RemoteCalendar::id`] the event lives in.
    pub calendar_id: String,
    /// Server-supplied ETag (or equivalent) for optimistic concurrency on
    /// updates and deletes. `None` when the source does not support it.
    pub etag: Option<String>,
    /// Cross-source dedup key. CalDAV's `UID`, Graph's `iCalUId`. Two
    /// `RemoteEvent`s with the same `ical_uid` from different sources
    /// represent the same real-world event.
    pub ical_uid: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub all_day: bool,
    pub location: Option<String>,
    /// Raw iCalendar `RRULE` string when the event recurs, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    /// Athen does not currently expand recurrences itself — the source's
    /// `list_events` is expected to return one occurrence per slot in the
    /// requested window.
    pub recurrence_rrule: Option<String>,
    /// Reminder lead times in minutes before start.
    pub reminder_minutes: Vec<i64>,
}

/// What a source supports. Used by the Settings UI to grey out actions and
/// by the sync loop to skip calls it knows will fail.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CalendarSourceCapabilities {
    pub read: bool,
    pub create: bool,
    pub update: bool,
    pub delete: bool,
    /// Provider can return free/busy slots within a time window across
    /// multiple attendees. Currently only Microsoft Graph
    /// (`findMeetingTimes`); CalDAV servers expose `free-busy-query` but
    /// we don't wire it yet.
    pub find_meeting_times: bool,
}

/// Producer-side adapter contract for one configured calendar account.
///
/// One instance == one logged-in user account on one provider. A user with
/// both a personal and work iCloud is two `CalDavSource` instances.
#[async_trait]
pub trait CalendarSource: Send + Sync {
    /// Stable identifier set when the source is configured in Settings.
    /// Persisted on each row in `calendar_events.source_id` so the sync
    /// loop can locate the matching adapter on the next pass.
    fn source_id(&self) -> &str;

    /// User-facing label, e.g. "iCloud (alex@me.com)".
    fn display_name(&self) -> &str;

    fn capabilities(&self) -> CalendarSourceCapabilities;

    /// Cheap auth/connectivity probe used by the Settings "Test" button.
    /// `Ok(())` means credentials authenticate; it does NOT pull events.
    async fn test_connection(&self) -> Result<()>;

    /// Enumerate the calendars this account exposes. Called once on setup
    /// (to populate the "which calendars to sync?" picker) and on demand
    /// from Settings when the user clicks "Refresh calendars".
    async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>>;

    /// Pull events from `calendar_id` whose time range overlaps
    /// `[start, end]`. Implementations should expand recurrences so the
    /// caller sees one entry per occurrence within the window.
    async fn list_events(
        &self,
        calendar_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<RemoteEvent>>;

    /// Create a new event on the remote. Returns the assigned `remote_id`
    /// and `etag` so the sync loop can stamp them onto the local row.
    /// Errors if `capabilities().create == false`.
    async fn create_event(
        &self,
        calendar_id: &str,
        event: &RemoteEvent,
    ) -> Result<(String, Option<String>)>;

    /// Update an existing event. When `if_match_etag` is `Some`, the
    /// remote MUST reject the write if its current etag differs (CalDAV
    /// `If-Match`, Graph `If-Match` header). Returns the new etag on success.
    async fn update_event(
        &self,
        calendar_id: &str,
        remote_id: &str,
        if_match_etag: Option<&str>,
        event: &RemoteEvent,
    ) -> Result<Option<String>>;

    async fn delete_event(
        &self,
        calendar_id: &str,
        remote_id: &str,
        if_match_etag: Option<&str>,
    ) -> Result<()>;
}
