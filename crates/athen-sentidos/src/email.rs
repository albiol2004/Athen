//! Email sense monitor.
//!
//! Stub implementation. A real version would integrate with Gmail, Outlook,
//! or IMAP APIs to poll for new messages.

use std::time::Duration;

use async_trait::async_trait;

use athen_core::config::AthenConfig;
use athen_core::error::Result;
use athen_core::event::SenseEvent;
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// Email sense monitor (stub).
///
/// When fully implemented this will poll an email provider API and convert
/// incoming messages into [`SenseEvent`]s with
/// [`RiskLevel::Caution`] since email is an external input channel.
pub struct EmailMonitor {
    poll_interval: Duration,
}

impl EmailMonitor {
    /// Create a new `EmailMonitor` with the default poll interval of 60 seconds.
    pub fn new() -> Self {
        Self {
            poll_interval: Duration::from_secs(60),
        }
    }

    /// Create an `EmailMonitor` with a custom poll interval.
    pub fn with_interval(poll_interval: Duration) -> Self {
        Self { poll_interval }
    }

    /// The risk level assigned to events from this source.
    pub fn source_risk() -> RiskLevel {
        RiskLevel::Caution
    }
}

impl Default for EmailMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SenseMonitor for EmailMonitor {
    fn sense_id(&self) -> &str {
        "email"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        tracing::info!("EmailMonitor initialized (stub)");
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        // Stub: real implementation would call Gmail/Outlook/IMAP API here.
        Ok(Vec::new())
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("EmailMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_id_is_email() {
        let monitor = EmailMonitor::new();
        assert_eq!(monitor.sense_id(), "email");
    }

    #[test]
    fn default_poll_interval_is_60s() {
        let monitor = EmailMonitor::new();
        assert_eq!(monitor.poll_interval(), Duration::from_secs(60));
    }

    #[test]
    fn custom_poll_interval() {
        let monitor = EmailMonitor::with_interval(Duration::from_secs(120));
        assert_eq!(monitor.poll_interval(), Duration::from_secs(120));
    }

    #[tokio::test]
    async fn poll_returns_empty() {
        let monitor = EmailMonitor::new();
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn source_risk_is_caution() {
        assert_eq!(EmailMonitor::source_risk(), RiskLevel::Caution);
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = EmailMonitor::new();
        monitor.shutdown().await.unwrap();
    }
}
