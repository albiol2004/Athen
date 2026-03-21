//! Timeout guard for task execution.

use std::time::{Duration, Instant};

use athen_core::traits::agent::TimeoutGuard;

/// Default timeout guard that tracks a deadline based on a fixed duration.
pub struct DefaultTimeoutGuard {
    deadline: Instant,
    duration: Duration,
}

impl DefaultTimeoutGuard {
    /// Create a new timeout guard with the given duration.
    /// The deadline is set to `Instant::now() + duration`.
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
            duration,
        }
    }

    /// Returns the configured total duration for this guard.
    pub fn total_duration(&self) -> Duration {
        self.duration
    }
}

impl TimeoutGuard for DefaultTimeoutGuard {
    fn remaining(&self) -> Duration {
        self.deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
    }

    fn is_expired(&self) -> bool {
        Instant::now() > self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_not_expired_immediately() {
        let guard = DefaultTimeoutGuard::new(Duration::from_secs(60));
        assert!(!guard.is_expired());
        assert!(guard.remaining() > Duration::ZERO);
    }

    #[test]
    fn test_timeout_expires() {
        let guard = DefaultTimeoutGuard::new(Duration::ZERO);
        // With zero duration, deadline is essentially now, so it should expire
        // after any passage of time.
        std::thread::sleep(Duration::from_millis(1));
        assert!(guard.is_expired());
        assert_eq!(guard.remaining(), Duration::ZERO);
    }

    #[test]
    fn test_total_duration() {
        let d = Duration::from_secs(42);
        let guard = DefaultTimeoutGuard::new(d);
        assert_eq!(guard.total_duration(), d);
    }
}
