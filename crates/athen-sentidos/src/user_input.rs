//! User input sense - direct commands from the authenticated user.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use athen_core::config::AthenConfig;
use athen_core::error::Result;
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

/// Sense monitor for direct user input commands.
///
/// The UI layer pushes messages through the [`sender()`] handle,
/// and the polling loop drains them as `SenseEvent`s.
pub struct UserInputMonitor {
    receiver: Mutex<mpsc::Receiver<String>>,
    sender_handle: mpsc::Sender<String>,
}

impl UserInputMonitor {
    /// Create a new `UserInputMonitor` with the given channel capacity.
    pub fn new(buffer: usize) -> Self {
        let (sender_handle, receiver) = mpsc::channel(buffer);
        Self {
            receiver: Mutex::new(receiver),
            sender_handle,
        }
    }

    /// Get a clone of the sender handle so the UI can push messages.
    pub fn sender(&self) -> mpsc::Sender<String> {
        self.sender_handle.clone()
    }
}

/// Convert a raw user command string into a [`SenseEvent`].
fn command_to_event(text: String) -> SenseEvent {
    SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::UserInput,
        kind: EventKind::Command,
        sender: None,
        content: NormalizedContent {
            summary: Some(text.clone()),
            body: serde_json::Value::String(text),
            attachments: Vec::new(),
        },
        source_risk: RiskLevel::Safe,
        raw_id: None,
    }
}

#[async_trait]
impl SenseMonitor for UserInputMonitor {
    fn sense_id(&self) -> &str {
        "user_input"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        tracing::info!("UserInputMonitor initialized");
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        let mut events = Vec::new();
        let mut rx = self.receiver.lock().await;
        // Drain all currently buffered messages without blocking.
        while let Ok(text) = rx.try_recv() {
            events.push(command_to_event(text));
        }
        Ok(events)
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(100)
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("UserInputMonitor shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poll_returns_empty_when_no_messages() {
        let monitor = UserInputMonitor::new(16);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_returns_event_for_single_message() {
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();
        tx.send("hello world".to_string()).await.unwrap();

        let events = monitor.poll().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, EventSource::UserInput);
        assert!(matches!(events[0].kind, EventKind::Command));
        assert!(events[0].sender.is_none());
        assert_eq!(events[0].source_risk, RiskLevel::Safe);
        assert_eq!(
            events[0].content.body,
            serde_json::Value::String("hello world".to_string())
        );
    }

    #[tokio::test]
    async fn poll_drains_multiple_queued_messages() {
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();
        tx.send("msg1".to_string()).await.unwrap();
        tx.send("msg2".to_string()).await.unwrap();
        tx.send("msg3".to_string()).await.unwrap();

        let events = monitor.poll().await.unwrap();
        assert_eq!(events.len(), 3);

        let bodies: Vec<_> = events
            .iter()
            .map(|e| e.content.body.as_str().unwrap().to_string())
            .collect();
        assert_eq!(bodies, vec!["msg1", "msg2", "msg3"]);
    }

    #[tokio::test]
    async fn poll_is_empty_after_drain() {
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();
        tx.send("once".to_string()).await.unwrap();

        let events = monitor.poll().await.unwrap();
        assert_eq!(events.len(), 1);

        // Second poll should be empty.
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn sense_id_is_user_input() {
        let monitor = UserInputMonitor::new(1);
        assert_eq!(monitor.sense_id(), "user_input");
    }

    #[test]
    fn poll_interval_is_100ms() {
        let monitor = UserInputMonitor::new(1);
        assert_eq!(monitor.poll_interval(), Duration::from_millis(100));
    }
}
