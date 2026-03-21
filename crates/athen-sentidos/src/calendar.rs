//! Calendar sense monitor.
//!
//! Stub implementation. A real version would integrate with Google Calendar,
//! Outlook Calendar, or CalDAV to poll for upcoming events and reminders.

use std::time::Duration;

use async_trait::async_trait;

use athen_core::config::AthenConfig;
use athen_core::error::Result;
use athen_core::event::SenseEvent;
use athen_core::traits::sense::SenseMonitor;

/// Calendar sense monitor (stub).
///
/// When fully implemented this will poll a calendar provider API and convert
/// upcoming events and reminders into [`SenseEvent`]s.
pub struct CalendarMonitor {
    poll_interval: Duration,
}

impl CalendarMonitor {
    /// Create a new `CalendarMonitor` with the default poll interval of 300 seconds (5 min).
    pub fn new() -> Self {
        Self {
            poll_interval: Duration::from_secs(300),
        }
    }

    /// Create a `CalendarMonitor` with a custom poll interval.
    pub fn with_interval(poll_interval: Duration) -> Self {
        Self { poll_interval }
    }
}

impl Default for CalendarMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SenseMonitor for CalendarMonitor {
    fn sense_id(&self) -> &str {
        "calendar"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        tracing::info!("CalendarMonitor initialized (stub)");
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        // Stub: real implementation would call Google Calendar / CalDAV API here.
        Ok(Vec::new())
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("CalendarMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_id_is_calendar() {
        let monitor = CalendarMonitor::new();
        assert_eq!(monitor.sense_id(), "calendar");
    }

    #[test]
    fn default_poll_interval_is_300s() {
        let monitor = CalendarMonitor::new();
        assert_eq!(monitor.poll_interval(), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn poll_returns_empty() {
        let monitor = CalendarMonitor::new();
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = CalendarMonitor::new();
        monitor.shutdown().await.unwrap();
    }
}
