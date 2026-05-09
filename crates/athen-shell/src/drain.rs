//! Drain gate for graceful shutdown of in-flight shell commands.
//!
//! On Windows the auto-updater needs to overwrite the bundled `nu.exe`
//! sidecar, but the OS holds a write lock on any binary currently
//! executing. Before triggering the update we close this gate and wait
//! for in-flight shell commands to finish so the OS releases its lock.
//! Once closed, subsequent shell invocations fail until the process
//! restarts — which is fine, the app is about to be replaced.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

pub struct DrainGate {
    closed: AtomicBool,
    in_flight: AtomicUsize,
    notify: Notify,
}

impl DrainGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    /// Acquire a permit for a shell call. Returns `None` if drain has
    /// begun — the caller should error out the call.
    pub fn enter(self: &Arc<Self>) -> Option<DrainPermit> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Some(DrainPermit {
            gate: Arc::clone(self),
        })
    }

    /// Close the gate and wait until all in-flight permits have been
    /// released, or `timeout` elapses. Returns true if drain completed.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.closed.store(true, Ordering::Release);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return true;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            tokio::select! {
                _ = &mut notified => continue,
                _ = tokio::time::sleep_until(deadline) => {
                    return self.in_flight.load(Ordering::Acquire) == 0;
                }
            }
        }
    }
}

pub struct DrainPermit {
    gate: Arc<DrainGate>,
}

impl Drop for DrainPermit {
    fn drop(&mut self) {
        if self.gate.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.gate.notify.notify_waiters();
        }
    }
}

static GLOBAL_GATE: OnceLock<Arc<DrainGate>> = OnceLock::new();

/// Process-wide gate shared by every Nushell/native shell call.
pub fn global_gate() -> &'static Arc<DrainGate> {
    GLOBAL_GATE.get_or_init(DrainGate::new)
}

/// Close the global gate and wait up to `timeout` for in-flight shell
/// commands to finish. Returns true if every call has exited.
///
/// Called by the auto-updater before swapping the `nu.exe` sidecar on
/// Windows so the OS isn't holding a write lock on it.
pub async fn drain_for_update(timeout: Duration) -> bool {
    global_gate().drain(timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn enter_returns_none_after_close() {
        let g = DrainGate::new();
        let _p = g.enter().expect("first enter ok");
        let g2 = Arc::clone(&g);
        let drainer = tokio::spawn(async move { g2.drain(Duration::from_millis(50)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(g.enter().is_none(), "should reject new entries");
        drop(_p);
        let drained = drainer.await.unwrap();
        assert!(drained, "drain completes once permit dropped");
    }

    #[tokio::test]
    async fn drain_times_out_when_permits_outlive_deadline() {
        let g = DrainGate::new();
        let _p = g.enter().expect("enter");
        let drained = g.drain(Duration::from_millis(20)).await;
        assert!(!drained, "should not drain while permit is held");
    }

    #[tokio::test]
    async fn many_concurrent_permits_all_drain() {
        let g = DrainGate::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..20 {
            let gate = Arc::clone(&g);
            let c = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                let _p = gate.enter().expect("enter");
                tokio::time::sleep(Duration::from_millis(20)).await;
                c.fetch_add(1, Ordering::AcqRel);
            }));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        let drained = g.drain(Duration::from_secs(1)).await;
        assert!(drained, "all 20 should drain inside 1s");
        assert_eq!(counter.load(Ordering::Acquire), 20);
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn permit_drop_decrements_in_flight() {
        let g = DrainGate::new();
        assert_eq!(g.in_flight.load(Ordering::Acquire), 0);
        let p = g.enter().expect("enter");
        assert_eq!(g.in_flight.load(Ordering::Acquire), 1);
        drop(p);
        assert_eq!(g.in_flight.load(Ordering::Acquire), 0);
    }
}
