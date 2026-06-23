//! `WakeupScheduler`: poll the store for due wake-ups, hand each to a sink,
//! advance `next_fire_at`. Tickwise testable.
//!
//! Loop responsibilities (per `docs/WAKEUPS.md`):
//! - On every tick, drain `list_due(now)`. Catch-up is implicit because
//!   `list_due` returns rows whose `next_fire_at <= now` — multiple missed
//!   slots of the same schedule appear as a single row, so they coalesce.
//! - For each fire: invoke the sink, then `mark_fired(id, now, next)` where
//!   `next = compute_next_fire(schedule, now)`. Passing `now` (not the
//!   missed `next_fire_at`) is what skips missed slots forward.
//! - A sink error is logged and the wake-up is still marked fired. The
//!   alternative (re-fire next tick) would busy-loop on a broken sink.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::FutureExt;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};
use uuid::Uuid;

use athen_core::error::Result;
use athen_core::traits::wakeup::{WakeupFireSink, WakeupStore};

use crate::compute::compute_next_fire;

/// What happened during a single tick. Returned by `tick()` for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// IDs that fired successfully.
    pub fired: Vec<Uuid>,
    /// IDs that fired but whose sink returned an error. They were still
    /// marked fired to avoid a busy-loop.
    pub fired_with_sink_error: Vec<Uuid>,
    /// IDs that have run their course (one-shot done, or schedule produced
    /// no next slot). `next_fire_at` cleared.
    pub finalized: Vec<Uuid>,
}

/// Scheduler driver. Holds a store + sink as type-erased trait objects so
/// the composition root can swap them.
pub struct WakeupScheduler<S: WakeupStore + ?Sized, K: WakeupFireSink + ?Sized> {
    store: Arc<S>,
    sink: Arc<K>,
}

impl<S: WakeupStore + ?Sized, K: WakeupFireSink + ?Sized> WakeupScheduler<S, K> {
    pub fn new(store: Arc<S>, sink: Arc<K>) -> Self {
        Self { store, sink }
    }

    /// Drain everything due at `now`, fire each, and advance schedules.
    /// Returns a `TickReport` so tests can assert exact behaviour.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickReport> {
        let due = self.store.list_due(now).await?;
        let mut report = TickReport::default();

        for w in due {
            // Compute the next fire BEFORE invoking the sink so a sink panic
            // / hang can't leave the row armed at a stale time.
            let next = compute_next_fire(&w.schedule, now);

            // Sink first; mark_fired even if the sink errors (avoids
            // busy-loop). The sink runs the full coordinator/executor stack
            // inline — a panic in there must NOT abort the scheduler loop and
            // silently kill all future wake-ups. `catch_unwind` contains the
            // panic per-fire, logs it, and treats it like a sink error (the
            // wake-up is still marked fired so we don't busy-loop on it).
            let sink_ok = match AssertUnwindSafe(self.sink.fire(&w, now))
                .catch_unwind()
                .await
            {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    error!(wakeup_id = %w.id, error = %e, "Wakeup sink failed; marking fired anyway");
                    false
                }
                Err(_panic) => {
                    error!(wakeup_id = %w.id, "Wakeup sink PANICKED; marking fired anyway, loop continues");
                    false
                }
            };

            if let Err(e) = self.store.mark_fired(w.id, now, next).await {
                // mark_fired failed — likely the row was deleted between
                // list_due and now. Log and move on.
                warn!(wakeup_id = %w.id, error = %e, "mark_fired failed (concurrent delete?)");
                continue;
            }

            if sink_ok {
                report.fired.push(w.id);
            } else {
                report.fired_with_sink_error.push(w.id);
            }
            if next.is_none() {
                report.finalized.push(w.id);
            }
        }

        debug!(
            fired = report.fired.len(),
            sink_errors = report.fired_with_sink_error.len(),
            finalized = report.finalized.len(),
            "Wakeup tick completed"
        );
        Ok(report)
    }

    /// Compute and persist `next_fire_at` for every enabled wake-up whose
    /// `next_fire_at` is currently `None` (freshly created or finalized).
    /// Idempotent. Called once at startup so freshly-created wake-ups
    /// without a next time get armed before the first tick.
    pub async fn arm_unscheduled(&self, now: DateTime<Utc>) -> Result<usize> {
        let all = self.store.list_all().await?;
        let mut armed = 0;
        for w in all {
            if !w.enabled || w.next_fire_at.is_some() {
                continue;
            }
            let next = compute_next_fire(&w.schedule, now);
            if next.is_some() {
                let mut updated = w.clone();
                updated.next_fire_at = next;
                if let Err(e) = self.store.update(&updated).await {
                    warn!(wakeup_id = %w.id, error = %e, "Failed to arm wakeup");
                    continue;
                }
                armed += 1;
            }
        }
        Ok(armed)
    }

    /// Spawn a polling loop that calls `tick` every `period`. Returns once
    /// `shutdown` resolves. Errors from individual ticks are logged and the
    /// loop continues.
    pub async fn run(&self, period: std::time::Duration, mut shutdown: oneshot::Receiver<()>) {
        let mut interval = tokio::time::interval(period);
        // Skip the immediate first tick so the caller's `arm_unscheduled`
        // step (if any) lands first.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    debug!("Wakeup scheduler shutting down");
                    return;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.tick(Utc::now()).await {
                        error!(error = %e, "Wakeup tick errored; loop continues");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use chrono::Duration;

    use athen_core::config::NotificationChannelKind;
    use athen_core::error::AthenError;
    use athen_core::wakeup::{AutonomyBand, Schedule, Wakeup, WakeupOrigin};

    /// In-memory mock store. Not meant to be production-correct, just
    /// faithful to the trait contract for the cases the scheduler exercises.
    #[derive(Default)]
    struct MockStore {
        rows: StdMutex<Vec<Wakeup>>,
    }

    #[async_trait]
    impl WakeupStore for MockStore {
        async fn create(&self, w: &Wakeup) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            if rows.iter().any(|r| r.id == w.id) {
                return Err(AthenError::Other(format!(
                    "Wakeup already exists: {}",
                    w.id
                )));
            }
            rows.push(w.clone());
            Ok(())
        }
        async fn update(&self, w: &Wakeup) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            match rows.iter_mut().find(|r| r.id == w.id) {
                Some(r) => {
                    *r = w.clone();
                    Ok(())
                }
                None => Err(AthenError::Other(format!("Wakeup not found: {}", w.id))),
            }
        }
        async fn delete(&self, id: Uuid) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|r| r.id != id);
            if rows.len() == before {
                Err(AthenError::Other(format!("Wakeup not found: {id}")))
            } else {
                Ok(())
            }
        }
        async fn get(&self, id: Uuid) -> Result<Option<Wakeup>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned())
        }
        async fn list_all(&self) -> Result<Vec<Wakeup>> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Wakeup>> {
            let mut due: Vec<Wakeup> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|w| w.enabled)
                .filter(|w| w.next_fire_at.map(|n| n <= now).unwrap_or(false))
                .cloned()
                .collect();
            due.sort_by_key(|w| w.next_fire_at);
            Ok(due)
        }
        async fn mark_fired(
            &self,
            id: Uuid,
            fired_at: DateTime<Utc>,
            next_fire_at: Option<DateTime<Utc>>,
        ) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| AthenError::Other(format!("Wakeup not found: {id}")))?;
            row.last_fired_at = Some(fired_at);
            row.next_fire_at = next_fire_at;
            Ok(())
        }
        async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| AthenError::Other(format!("Wakeup not found: {id}")))?;
            row.enabled = enabled;
            Ok(())
        }
    }

    /// Records every fire it sees.
    #[derive(Default)]
    struct RecordingSink {
        fires: StdMutex<Vec<(Uuid, DateTime<Utc>)>>,
    }

    #[async_trait]
    impl WakeupFireSink for RecordingSink {
        async fn fire(&self, wakeup: &Wakeup, fired_at: DateTime<Utc>) -> Result<()> {
            self.fires.lock().unwrap().push((wakeup.id, fired_at));
            Ok(())
        }
    }

    /// Always errors — used to verify mark_fired still happens.
    struct FailingSink;

    #[async_trait]
    impl WakeupFireSink for FailingSink {
        async fn fire(&self, _wakeup: &Wakeup, _fired_at: DateTime<Utc>) -> Result<()> {
            Err(AthenError::Other("intentional sink failure".into()))
        }
    }

    /// Always panics — used to verify the scheduler loop survives a panic in
    /// the (inline) coordinator/executor stack and still marks fired.
    struct PanickingSink;

    #[async_trait]
    impl WakeupFireSink for PanickingSink {
        async fn fire(&self, _wakeup: &Wakeup, _fired_at: DateTime<Utc>) -> Result<()> {
            panic!("intentional sink panic");
        }
    }

    fn mk_oneshot(at: DateTime<Utc>) -> Wakeup {
        Wakeup {
            id: Uuid::new_v4(),
            schedule: Schedule::OneShot { at },
            instruction: "x".into(),
            autonomy: AutonomyBand::SafeOnly,
            preferred_channel: Some(NotificationChannelKind::InApp),
            tool_allowlist: None,
            contact_allowlist: None,
            inherit_restrictions: true,
            profile: "assistant".into(),
            arc_id: None,
            origin: WakeupOrigin::User,
            created_at: Utc::now(),
            last_fired_at: None,
            next_fire_at: Some(at),
            enabled: true,
        }
    }

    fn mk_interval(every: u64, anchor: DateTime<Utc>, next: DateTime<Utc>) -> Wakeup {
        let mut w = mk_oneshot(next);
        w.schedule = Schedule::Interval {
            every_seconds: every,
            anchor,
        };
        w.next_fire_at = Some(next);
        w
    }

    #[tokio::test]
    async fn tick_fires_due_wakeups_and_advances() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink.clone());

        let now = Utc::now();
        let w1 = mk_oneshot(now - Duration::minutes(5));
        let w2 = mk_oneshot(now + Duration::hours(1));
        store.create(&w1).await.unwrap();
        store.create(&w2).await.unwrap();

        let report = sched.tick(now).await.unwrap();
        assert_eq!(report.fired, vec![w1.id]);
        assert_eq!(report.finalized, vec![w1.id]);
        // sink saw it
        let fires = sink.fires.lock().unwrap().clone();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].0, w1.id);

        // The fired one-shot should have next_fire_at = None now.
        let after = store.get(w1.id).await.unwrap().unwrap();
        assert!(after.next_fire_at.is_none());
        assert_eq!(after.last_fired_at.unwrap(), now);

        // The future one was untouched.
        let untouched = store.get(w2.id).await.unwrap().unwrap();
        assert_eq!(untouched.next_fire_at.unwrap(), now + Duration::hours(1));
    }

    #[tokio::test]
    async fn tick_advances_interval_to_next_grid_after_now() {
        // Exercises the catch-up coalescing rule: missed many slots, but
        // tick advances next_fire_at to the next slot strictly after `now`,
        // not the next slot after the missed time. So one fire, then the
        // schedule resumes from the present.
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink.clone());

        // Hourly schedule anchored 100 hours ago, with stored next_fire_at
        // pointing at an old missed slot.
        let now = Utc::now();
        let anchor = now - Duration::hours(100);
        let stale_next = now - Duration::hours(99);
        let w = mk_interval(3600, anchor, stale_next);
        store.create(&w).await.unwrap();

        let report = sched.tick(now).await.unwrap();
        assert_eq!(report.fired, vec![w.id]);
        assert!(report.finalized.is_empty());

        let after = store.get(w.id).await.unwrap().unwrap();
        let next = after.next_fire_at.unwrap();
        // The new next is strictly > now and is on the hourly grid.
        assert!(next > now);
        assert!(next - now <= Duration::hours(1));
        // Only ONE fire even though 99 slots were missed — coalescing.
        assert_eq!(sink.fires.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tick_skips_disabled_wakeups() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink.clone());

        let now = Utc::now();
        let mut w = mk_oneshot(now - Duration::minutes(1));
        w.enabled = false;
        store.create(&w).await.unwrap();

        let report = sched.tick(now).await.unwrap();
        assert!(report.fired.is_empty());
        assert!(sink.fires.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tick_marks_fired_even_when_sink_errors() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(FailingSink);
        let sched = WakeupScheduler::new(store.clone(), sink);

        let now = Utc::now();
        let w = mk_oneshot(now - Duration::minutes(1));
        store.create(&w).await.unwrap();

        let report = sched.tick(now).await.unwrap();
        assert_eq!(report.fired_with_sink_error, vec![w.id]);
        assert!(report.fired.is_empty());
        // Critically: the row is now marked fired so we don't busy-loop.
        let after = store.get(w.id).await.unwrap().unwrap();
        assert!(after.last_fired_at.is_some());
        assert!(after.next_fire_at.is_none());
    }

    #[tokio::test]
    async fn tick_survives_panicking_sink_and_marks_fired() {
        // A panic in the inline sink (coordinator/executor stack) must be
        // contained per-fire: the tick returns normally, the wake-up is
        // marked fired (no busy-loop), and a second due wake-up in the same
        // tick still fires.
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(PanickingSink);
        let sched = WakeupScheduler::new(store.clone(), sink);

        let now = Utc::now();
        let w1 = mk_oneshot(now - Duration::minutes(2));
        let w2 = mk_oneshot(now - Duration::minutes(1));
        store.create(&w1).await.unwrap();
        store.create(&w2).await.unwrap();

        // The panic is caught; the tick itself does not panic/unwind.
        let report = sched.tick(now).await.unwrap();
        // Both treated as sink errors (panic == fire failure), still marked.
        assert!(report.fired.is_empty());
        assert_eq!(report.fired_with_sink_error.len(), 2);

        for id in [w1.id, w2.id] {
            let after = store.get(id).await.unwrap().unwrap();
            assert!(after.last_fired_at.is_some());
            assert!(after.next_fire_at.is_none());
        }
    }

    #[tokio::test]
    async fn arm_unscheduled_fills_in_next_fire_at_for_fresh_rows() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink);

        let now = Utc::now();
        // Fresh wake-up with next_fire_at = None (schedule says future
        // anchor, but caller didn't compute it yet).
        let mut w = mk_interval(3600, now + Duration::hours(2), now);
        w.next_fire_at = None;
        store.create(&w).await.unwrap();

        let armed = sched.arm_unscheduled(now).await.unwrap();
        assert_eq!(armed, 1);
        let after = store.get(w.id).await.unwrap().unwrap();
        assert_eq!(after.next_fire_at.unwrap(), now + Duration::hours(2));
    }

    #[tokio::test]
    async fn arm_unscheduled_skips_disabled_and_already_armed() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink);

        let now = Utc::now();
        let mut disabled = mk_oneshot(now + Duration::hours(1));
        disabled.enabled = false;
        disabled.next_fire_at = None;
        let armed_already = mk_oneshot(now + Duration::hours(2));
        // armed_already.next_fire_at is Some by default
        store.create(&disabled).await.unwrap();
        store.create(&armed_already).await.unwrap();

        let n = sched.arm_unscheduled(now).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn tick_fires_due_in_order_of_next_fire_at() {
        let store = Arc::new(MockStore::default());
        let sink = Arc::new(RecordingSink::default());
        let sched = WakeupScheduler::new(store.clone(), sink.clone());

        let now = Utc::now();
        let earlier = mk_oneshot(now - Duration::minutes(10));
        let later = mk_oneshot(now - Duration::minutes(1));
        store.create(&later).await.unwrap();
        store.create(&earlier).await.unwrap();

        let _ = sched.tick(now).await.unwrap();
        let order: Vec<Uuid> = sink
            .fires
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(order, vec![earlier.id, later.id]);
    }
}
