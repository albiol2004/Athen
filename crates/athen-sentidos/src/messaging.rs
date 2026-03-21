//! Messaging sense monitor (iMessage, WhatsApp, etc).
//!
//! Stub implementation. A real version would integrate with messaging
//! platform APIs to poll for new messages.

use std::time::Duration;

use async_trait::async_trait;

use athen_core::config::AthenConfig;
use athen_core::error::Result;
use athen_core::event::SenseEvent;
use athen_core::traits::sense::SenseMonitor;

/// Messaging sense monitor (stub).
///
/// When fully implemented this will poll messaging platforms (iMessage,
/// WhatsApp, etc.) and convert incoming messages into [`SenseEvent`]s.
pub struct MessagingMonitor {
    poll_interval: Duration,
}

impl MessagingMonitor {
    /// Create a new `MessagingMonitor` with the default poll interval of 30 seconds.
    pub fn new() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
        }
    }

    /// Create a `MessagingMonitor` with a custom poll interval.
    pub fn with_interval(poll_interval: Duration) -> Self {
        Self { poll_interval }
    }
}

impl Default for MessagingMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SenseMonitor for MessagingMonitor {
    fn sense_id(&self) -> &str {
        "messaging"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        tracing::info!("MessagingMonitor initialized (stub)");
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        // Stub: real implementation would call iMessage/WhatsApp APIs here.
        Ok(Vec::new())
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("MessagingMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_id_is_messaging() {
        let monitor = MessagingMonitor::new();
        assert_eq!(monitor.sense_id(), "messaging");
    }

    #[test]
    fn default_poll_interval_is_30s() {
        let monitor = MessagingMonitor::new();
        assert_eq!(monitor.poll_interval(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn poll_returns_empty() {
        let monitor = MessagingMonitor::new();
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = MessagingMonitor::new();
        monitor.shutdown().await.unwrap();
    }
}
