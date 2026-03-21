use async_trait::async_trait;
use std::time::Duration;

use crate::config::AthenConfig;
use crate::error::Result;
use crate::event::SenseEvent;

/// A sense monitor polls an external source and produces normalized events.
/// Each monitor runs in its own process and sends events over IPC.
#[async_trait]
pub trait SenseMonitor: Send + Sync + 'static {
    /// Unique identifier for this sense type (e.g., "email", "calendar").
    fn sense_id(&self) -> &str;

    /// Initialize the monitor with its configuration.
    async fn init(&mut self, config: &AthenConfig) -> Result<()>;

    /// Poll once. Returns zero or more normalized events.
    async fn poll(&self) -> Result<Vec<SenseEvent>>;

    /// Polling interval hint (the process runner respects this).
    fn poll_interval(&self) -> Duration;

    /// Graceful shutdown.
    async fn shutdown(&self) -> Result<()>;
}
