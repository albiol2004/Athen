use async_trait::async_trait;

use crate::config::NotificationChannelKind;
use crate::error::Result;
use crate::notification::{DeliveryResult, Notification};

/// A single notification delivery channel (InApp, Telegram, etc.).
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    fn channel_kind(&self) -> NotificationChannelKind;
    async fn send(&self, notification: &Notification) -> Result<DeliveryResult>;
}
