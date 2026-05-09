//! Wake-up persistence + dispatch ports. Persistence lives in
//! `athen-persistence`; the scheduler driver lives in `athen-scheduler`;
//! the fire sink is wired in `athen-app` (Phase 3 — coordinator-backed).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::Result;
use crate::wakeup::Wakeup;

/// Storage for wake-ups (scheduled / recurring / one-shot triggers).
///
/// The store is intentionally simple: CRUD plus a `list_due` helper for the
/// scheduler. Computing `next_fire_at` (cron expansion, interval math) is
/// the scheduler's job, not the store's — the store is just a typed table.
#[async_trait]
pub trait WakeupStore: Send + Sync {
    /// Insert a new wake-up. Errors if `id` is already present.
    async fn create(&self, wakeup: &Wakeup) -> Result<()>;

    /// Replace an existing wake-up by id. Errors if missing.
    async fn update(&self, wakeup: &Wakeup) -> Result<()>;

    /// Delete by id. Errors if missing.
    async fn delete(&self, id: Uuid) -> Result<()>;

    /// Look up by id. Returns `None` if absent.
    async fn get(&self, id: Uuid) -> Result<Option<Wakeup>>;

    /// List every wake-up regardless of state, ordered by `created_at` desc
    /// (newest first). Used by the visibility tab.
    async fn list_all(&self) -> Result<Vec<Wakeup>>;

    /// List wake-ups that are enabled and have `next_fire_at <= now`.
    /// Ordered by `next_fire_at` ascending (earliest due first). The
    /// scheduler polls this on its tick.
    async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Wakeup>>;

    /// Atomically record that a wake-up just fired and write its newly
    /// computed `next_fire_at`. Pass `None` for `next_fire_at` when the
    /// wake-up has run its course (one-shot done, or schedule disabled).
    /// Errors if the wake-up id is missing.
    async fn mark_fired(
        &self,
        id: Uuid,
        fired_at: DateTime<Utc>,
        next_fire_at: Option<DateTime<Utc>>,
    ) -> Result<()>;

    /// Toggle the `enabled` flag without touching anything else. Errors if
    /// missing.
    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()>;
}

/// Sink that consumes wake-up fires. The scheduler computes "this wake-up is
/// due now," advances `next_fire_at`, and hands the (cloned) `Wakeup` to the
/// sink. Production wires this to the coordinator (Phase 3) so a fire turns
/// into a synthetic sense event. Tests wire a channel-backed sink.
///
/// Sinks should be cheap and non-blocking — typical implementation pushes
/// onto an mpsc channel and returns. A sink error is logged but never aborts
/// the scheduler loop; the wakeup is still marked fired so we don't busy-loop
/// retrying a broken sink.
#[async_trait]
pub trait WakeupFireSink: Send + Sync {
    async fn fire(&self, wakeup: &Wakeup, fired_at: DateTime<Utc>) -> Result<()>;
}
