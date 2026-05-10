//! In-process sliding-window rate limiter for the `http_request` tool.
//!
//! Per-endpoint, per-process. Window is fixed at 60s — `RateLimit` only
//! exposes `requests_per_minute`, so a single window covers every preset.
//! Lives on `AppState` so every per-arc registry sees the same counters;
//! crossing the limit returns a structured error and is *not* persisted.
//!
//! The HashMap grows unboundedly with new endpoint ids in theory; in
//! practice users register tens of endpoints, not thousands. A periodic
//! shrink would be wasteful at this scale.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

const WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct HttpRateLimiter {
    inner: Mutex<HashMap<Uuid, VecDeque<Instant>>>,
}

/// Outcome of the pre-flight check. `Allowed` means the request may go
/// through — the timestamp is not recorded until [`HttpRateLimiter::record`]
/// is called, so failed calls don't burn quota.
#[derive(Debug, PartialEq, Eq)]
pub enum RateCheck {
    Allowed,
    Exceeded {
        recent_calls: u32,
        limit_per_minute: u32,
        retry_in_secs: u64,
    },
}

impl HttpRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a call to `endpoint` would exceed `limit_per_minute`.
    /// `limit_per_minute = 0` means "no limit configured".
    pub fn check(&self, endpoint: Uuid, limit_per_minute: u32) -> RateCheck {
        if limit_per_minute == 0 {
            return RateCheck::Allowed;
        }
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let entry = map.entry(endpoint).or_default();
        let cutoff = Instant::now() - WINDOW;
        while entry.front().map(|t| *t < cutoff).unwrap_or(false) {
            entry.pop_front();
        }
        let recent = entry.len() as u32;
        if recent >= limit_per_minute {
            // The oldest still-in-window timestamp tells us when the
            // bucket frees up. Always >= 1s in practice.
            let retry_in_secs = entry
                .front()
                .map(|t| WINDOW.saturating_sub(t.elapsed()).as_secs().max(1))
                .unwrap_or(60);
            RateCheck::Exceeded {
                recent_calls: recent,
                limit_per_minute,
                retry_in_secs,
            }
        } else {
            RateCheck::Allowed
        }
    }

    /// Record a successful (or attempted) call. Pair with `check`: only
    /// call `record` when the request actually went out.
    pub fn record(&self, endpoint: Uuid) {
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        map.entry(endpoint).or_default().push_back(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_limit_means_unlimited() {
        let rl = HttpRateLimiter::new();
        let id = Uuid::new_v4();
        for _ in 0..1000 {
            assert_eq!(rl.check(id, 0), RateCheck::Allowed);
            rl.record(id);
        }
    }

    #[test]
    fn enforces_limit_within_window() {
        let rl = HttpRateLimiter::new();
        let id = Uuid::new_v4();
        for _ in 0..3 {
            assert_eq!(rl.check(id, 3), RateCheck::Allowed);
            rl.record(id);
        }
        match rl.check(id, 3) {
            RateCheck::Exceeded {
                recent_calls,
                limit_per_minute,
                ..
            } => {
                assert_eq!(recent_calls, 3);
                assert_eq!(limit_per_minute, 3);
            }
            other => panic!("expected Exceeded, got {other:?}"),
        }
    }

    #[test]
    fn endpoints_are_isolated() {
        let rl = HttpRateLimiter::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        rl.record(a);
        rl.record(a);
        // a hits its 2-rpm cap.
        assert!(matches!(rl.check(a, 2), RateCheck::Exceeded { .. }));
        // b is independent.
        assert_eq!(rl.check(b, 2), RateCheck::Allowed);
    }
}
