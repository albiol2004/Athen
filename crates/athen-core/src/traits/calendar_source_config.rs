//! Persistence port for [`CalendarSourceConfig`] rows.
//!
//! Mirrors the shape of [`HttpEndpointStore`](crate::traits::http_endpoint)
//! — opaque CRUD with a small set of mutating shortcuts so the sync loop
//! can stamp `last_sync_at` / `last_sync_error` without round-tripping
//! the whole row.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::calendar_source_config::CalendarSourceConfig;
use crate::error::Result;

#[async_trait]
pub trait CalendarSourceConfigStore: Send + Sync {
    async fn list(&self) -> Result<Vec<CalendarSourceConfig>>;

    async fn get(&self, id: Uuid) -> Result<Option<CalendarSourceConfig>>;

    /// Insert or replace. `created_at` is preserved across replaces.
    async fn upsert(&self, config: &CalendarSourceConfig) -> Result<()>;

    async fn delete(&self, id: Uuid) -> Result<()>;

    /// Toggle without round-tripping the whole row.
    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()>;

    /// Replace the `selected_calendars` list after the user picks which
    /// remote calendars to sync.
    async fn set_selected_calendars(&self, id: Uuid, calendars: &[String]) -> Result<()>;

    /// Stamp `last_sync_at` and clear `last_sync_error`. Called from the
    /// sync loop on a successful pass.
    async fn record_sync_success(&self, id: Uuid, at: DateTime<Utc>) -> Result<()>;

    /// Record a sync failure without touching `last_sync_at` (so the
    /// "stale by" indicator stays accurate).
    async fn record_sync_error(&self, id: Uuid, error: &str) -> Result<()>;
}
