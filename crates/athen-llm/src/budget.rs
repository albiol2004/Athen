//! Token and cost tracking per provider.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use athen_core::llm::{BudgetStatus, TokenUsage};

/// Tracks daily spending and token usage, enforcing budget limits.
pub struct BudgetTracker {
    daily_limit_usd: Option<f64>,
    warn_at_percent: f64,
    usage_today: Mutex<f64>,
    tokens_today: AtomicU64,
    day_start: Mutex<DateTime<Utc>>,
}

impl BudgetTracker {
    /// Create a new budget tracker with optional daily limit.
    pub fn new(daily_limit_usd: Option<f64>) -> Self {
        Self {
            daily_limit_usd,
            warn_at_percent: 80.0,
            usage_today: Mutex::new(0.0),
            tokens_today: AtomicU64::new(0),
            day_start: Mutex::new(today_start()),
        }
    }

    /// Create with a custom warning threshold percentage (0-100).
    pub fn with_warn_percent(mut self, percent: f64) -> Self {
        self.warn_at_percent = percent;
        self
    }

    /// Check if we have enough budget for the estimated cost.
    /// Returns `true` if we can afford it (or if there is no limit).
    pub fn can_afford(&self, estimated_cost: f64) -> bool {
        self.maybe_reset_day();
        match self.daily_limit_usd {
            None => true,
            Some(limit) => {
                let spent = *self.usage_today.lock().unwrap();
                if limit <= 0.0 {
                    return false;
                }
                spent + estimated_cost <= limit
            }
        }
    }

    /// Record usage from a completed LLM call.
    pub fn record_usage(&self, usage: &TokenUsage) {
        self.maybe_reset_day();
        if let Some(cost) = usage.estimated_cost_usd {
            let mut spent = self.usage_today.lock().unwrap();
            *spent += cost;
        }
        self.tokens_today
            .fetch_add(usage.total_tokens as u64, Ordering::Relaxed);
    }

    /// Get current budget status.
    pub fn status(&self) -> BudgetStatus {
        self.maybe_reset_day();
        let spent = *self.usage_today.lock().unwrap();
        let remaining = self.daily_limit_usd.map(|limit| (limit - spent).max(0.0));
        BudgetStatus {
            daily_limit_usd: self.daily_limit_usd,
            spent_today_usd: spent,
            remaining_usd: remaining,
            tokens_used_today: self.tokens_today.load(Ordering::Relaxed),
        }
    }

    /// Check if spending has exceeded the warning threshold.
    pub fn is_warning(&self) -> bool {
        match self.daily_limit_usd {
            None => false,
            Some(limit) => {
                let spent = *self.usage_today.lock().unwrap();
                let percent = (spent / limit) * 100.0;
                percent >= self.warn_at_percent
            }
        }
    }

    /// Reset counters if the day has rolled over (midnight UTC).
    fn maybe_reset_day(&self) {
        let now = Utc::now();
        let mut day_start = self.day_start.lock().unwrap();
        let current_start = today_start();
        if current_start > *day_start {
            *day_start = current_start;
            *self.usage_today.lock().unwrap() = 0.0;
            self.tokens_today.store(0, Ordering::Relaxed);
        }
        drop(day_start);
        let _ = now; // suppress unused warning
    }
}

/// Get the start of today (midnight UTC).
fn today_start() -> DateTime<Utc> {
    let now = Utc::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unlimited_budget_always_affords() {
        let tracker = BudgetTracker::new(None);
        assert!(tracker.can_afford(999_999.0));
    }

    #[test]
    fn test_budget_tracking_and_enforcement() {
        let tracker = BudgetTracker::new(Some(10.0));
        assert!(tracker.can_afford(5.0));

        // Record some usage
        tracker.record_usage(&TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: Some(7.0),
        });

        // Should not afford another 5.0 (7 + 5 > 10)
        assert!(!tracker.can_afford(5.0));
        // But can afford 3.0 (7 + 3 = 10)
        assert!(tracker.can_afford(3.0));
    }

    #[test]
    fn test_budget_status() {
        let tracker = BudgetTracker::new(Some(20.0));
        tracker.record_usage(&TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 100,
            total_tokens: 300,
            estimated_cost_usd: Some(5.0),
        });

        let status = tracker.status();
        assert_eq!(status.daily_limit_usd, Some(20.0));
        assert!((status.spent_today_usd - 5.0).abs() < f64::EPSILON);
        assert!((status.remaining_usd.unwrap() - 15.0).abs() < f64::EPSILON);
        assert_eq!(status.tokens_used_today, 300);
    }

    #[test]
    fn test_warning_threshold() {
        let tracker = BudgetTracker::new(Some(100.0)).with_warn_percent(80.0);
        assert!(!tracker.is_warning());

        tracker.record_usage(&TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: Some(80.0),
        });
        assert!(tracker.is_warning());
    }

    #[test]
    fn test_no_cost_usage_still_tracks_tokens() {
        let tracker = BudgetTracker::new(Some(10.0));
        tracker.record_usage(&TokenUsage {
            prompt_tokens: 500,
            completion_tokens: 200,
            total_tokens: 700,
            estimated_cost_usd: None,
        });

        let status = tracker.status();
        assert_eq!(status.tokens_used_today, 700);
        assert!((status.spent_today_usd).abs() < f64::EPSILON);
    }
}
