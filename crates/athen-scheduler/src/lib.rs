//! Wake-up scheduler.
//!
//! Two pieces:
//!
//! - [`compute_next_fire`]: pure function that, given a `Schedule` and a
//!   reference timestamp, returns the next fire time strictly after the
//!   reference. No IO, no clock, fully deterministic — heart of the unit
//!   tests.
//! - [`WakeupScheduler`]: driver that polls the `WakeupStore` for due
//!   wake-ups and hands them to a `WakeupFireSink`. `tick(now)` is the
//!   testable unit; `run(...)` wraps tick in a tokio loop with a
//!   shutdown signal.
//!
//! Catch-up policy: per `docs/WAKEUPS.md`, missed fires of the same
//! schedule coalesce. The scheduler achieves this naturally — `list_due`
//! returns one row per wake-up regardless of how many slots were missed,
//! and `mark_fired` advances `next_fire_at` to the next slot strictly
//! after "now," not the next slot after the missed time.

pub mod compute;
pub mod scheduler;

pub use compute::compute_next_fire;
pub use scheduler::{TickReport, WakeupScheduler};
