//! In-memory rate limiting: login brute-force throttle + per-user
//! request buckets.
//!
//! Deliberately process-local (no persistence, no Redis) — the panel is a
//! single binary and a restart resetting limits is fine. Two mechanisms:
//!
//! - **Login throttle** (`LoginThrottle`): per-username consecutive-failure
//!   lockout with exponential backoff, plus a coarse global attempts/min
//!   cap so username enumeration can't sidestep the per-name state.
//!   Keyed by username, not client IP: the panel usually sits behind a
//!   TLS proxy, and trusting `X-Forwarded-For` without config is worse
//!   than not using IPs at all.
//! - **Request buckets** (`UserBuckets`): per-user token bucket applied to
//!   every session-gated request. Generous on purpose — it exists to stop
//!   runaway clients and scripted abuse, not to meter normal chat traffic
//!   (SSE/long-poll connections cost one token each, however long-lived).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive failures tolerated before lockouts start.
const FAIL_THRESHOLD: u32 = 5;
/// First lockout length; doubles per further failure, capped below.
const LOCKOUT_BASE: Duration = Duration::from_secs(30);
const LOCKOUT_MAX: Duration = Duration::from_secs(3600);
/// Global cap on login attempts (any username) per minute.
const GLOBAL_LOGIN_PER_MIN: u32 = 30;

#[derive(Default)]
struct FailState {
    consecutive: u32,
    locked_until: Option<Instant>,
}

#[derive(Default)]
pub struct LoginThrottle {
    by_user: Mutex<HashMap<String, FailState>>,
    /// (window start, attempts in window) for the global cap.
    global: Mutex<(Option<Instant>, u32)>,
}

impl LoginThrottle {
    /// Gate an incoming login attempt BEFORE verifying the password.
    /// `Err(retry_after)` means reject with 429 without touching argon2.
    pub fn check(&self, username: &str, now: Instant) -> Result<(), Duration> {
        {
            let mut g = self.global.lock().expect("login throttle poisoned");
            match g.0 {
                Some(start) if now.duration_since(start) < Duration::from_secs(60) => {
                    if g.1 >= GLOBAL_LOGIN_PER_MIN {
                        let retry = Duration::from_secs(60) - now.duration_since(start);
                        return Err(retry);
                    }
                    g.1 += 1;
                }
                _ => *g = (Some(now), 1),
            }
        }
        let map = self.by_user.lock().expect("login throttle poisoned");
        if let Some(st) = map.get(&username.to_lowercase()) {
            if let Some(until) = st.locked_until {
                if until > now {
                    return Err(until - now);
                }
            }
        }
        Ok(())
    }

    /// Record a failed password for `username`. From the threshold on,
    /// each failure locks the name out for `LOCKOUT_BASE * 2^extra`.
    pub fn record_failure(&self, username: &str, now: Instant) {
        let mut map = self.by_user.lock().expect("login throttle poisoned");
        let st = map.entry(username.to_lowercase()).or_default();
        st.consecutive += 1;
        if st.consecutive >= FAIL_THRESHOLD {
            let exp = (st.consecutive - FAIL_THRESHOLD).min(7); // 30s … capped
            let lockout = LOCKOUT_BASE
                .saturating_mul(1 << exp)
                .min(LOCKOUT_MAX);
            st.locked_until = Some(now + lockout);
        }
    }

    /// Successful login clears the failure state.
    pub fn record_success(&self, username: &str) {
        self.by_user
            .lock()
            .expect("login throttle poisoned")
            .remove(&username.to_lowercase());
    }
}

/// Sustained per-user request rate (tokens refilled per second).
const BUCKET_REFILL_PER_SEC: f64 = 5.0;
/// Burst capacity.
const BUCKET_CAPACITY: f64 = 300.0;

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub struct UserBuckets {
    by_user: Mutex<HashMap<String, Bucket>>,
}

impl UserBuckets {
    /// Take one token for `user_id`; `false` = over the limit (429).
    pub fn allow(&self, user_id: &str, now: Instant) -> bool {
        let mut map = self.by_user.lock().expect("user buckets poisoned");
        // Opportunistic prune so the map can't grow unbounded across
        // many users/sessions: drop buckets idle long enough to be full.
        if map.len() > 1024 {
            let idle = Duration::from_secs_f64(BUCKET_CAPACITY / BUCKET_REFILL_PER_SEC);
            map.retain(|_, b| now.duration_since(b.last) < idle);
        }
        let b = map.entry(user_id.to_string()).or_insert(Bucket {
            tokens: BUCKET_CAPACITY,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * BUCKET_REFILL_PER_SEC).min(BUCKET_CAPACITY);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_locks_after_threshold_and_recovers() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD - 1 {
            t.record_failure("Alice", now);
        }
        assert!(t.check("alice", now).is_ok(), "below threshold stays open");
        t.record_failure("ALICE", now); // case-insensitive key
        let retry = t.check("alice", now).expect_err("locked at threshold");
        assert!(retry >= LOCKOUT_BASE - Duration::from_secs(1));
        // Lockout expires with time…
        assert!(t.check("alice", now + LOCKOUT_BASE + Duration::from_secs(1)).is_ok());
        // …and success clears everything immediately.
        t.record_failure("alice", now);
        t.record_success("alice");
        for _ in 0..FAIL_THRESHOLD - 1 {
            t.record_failure("alice", now);
        }
        assert!(t.check("alice", now).is_ok());
    }

    #[test]
    fn lockout_backoff_grows_and_caps() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD + 20 {
            t.record_failure("bob", now);
        }
        let retry = t.check("bob", now).expect_err("locked");
        assert!(retry <= LOCKOUT_MAX);
        assert!(retry > LOCKOUT_BASE * 4, "backoff grew past the base");
    }

    #[test]
    fn global_login_cap() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for i in 0..GLOBAL_LOGIN_PER_MIN {
            assert!(t.check(&format!("user{i}"), now).is_ok());
        }
        assert!(t.check("one-more", now).is_err(), "global cap reached");
        // New window after 60s.
        assert!(t
            .check("one-more", now + Duration::from_secs(61))
            .is_ok());
    }

    #[test]
    fn bucket_allows_burst_then_refills() {
        let b = UserBuckets::default();
        let now = Instant::now();
        for _ in 0..BUCKET_CAPACITY as u32 {
            assert!(b.allow("u1", now));
        }
        assert!(!b.allow("u1", now), "burst exhausted");
        // Other users unaffected.
        assert!(b.allow("u2", now));
        // ~1s refills BUCKET_REFILL_PER_SEC tokens.
        let later = now + Duration::from_secs(1);
        for _ in 0..BUCKET_REFILL_PER_SEC as u32 {
            assert!(b.allow("u1", later));
        }
        assert!(!b.allow("u1", later));
    }
}
