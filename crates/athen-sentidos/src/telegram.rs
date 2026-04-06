//! Telegram Bot sense monitor.
//!
//! Polls the Telegram Bot API via raw HTTP (`reqwest`) for new messages
//! and converts each into a [`SenseEvent`] with [`EventSource::Messaging`].
//! Uses the `getUpdates` long-polling endpoint with offset tracking to
//! avoid processing the same message twice.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use uuid::Uuid;

use athen_core::config::{AthenConfig, TelegramConfig};
use athen_core::error::{AthenError, Result};
use athen_core::event::{
    EventKind, EventSource, NormalizedContent, SenderInfo, SenseEvent,
};
use athen_core::risk::RiskLevel;
use athen_core::traits::sense::SenseMonitor;

// ---------------------------------------------------------------------------
// Telegram Bot API response types (minimal)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TelegramResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub date: i64,
    pub text: Option<String>,
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
}

// ---------------------------------------------------------------------------
// TelegramMonitor
// ---------------------------------------------------------------------------

/// Telegram Bot API sense monitor.
///
/// Polls `getUpdates` for new messages, converts them to [`SenseEvent`]s,
/// and tracks the last processed `update_id` to avoid duplicates.
pub struct TelegramMonitor {
    config: TelegramConfig,
    client: reqwest::Client,
    last_update_id: Mutex<Option<i64>>,
}

impl TelegramMonitor {
    /// Create a new `TelegramMonitor` from the given config.
    pub fn new(config: TelegramConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            last_update_id: Mutex::new(None),
        }
    }

    /// Base URL for the Telegram Bot API.
    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.config.bot_token, method
        )
    }

    /// Convert a list of Telegram updates into [`SenseEvent`]s.
    ///
    /// This method is public so it can be tested in isolation without
    /// making HTTP calls.
    pub fn process_updates(&self, updates: Vec<TelegramUpdate>) -> Vec<SenseEvent> {
        let mut events = Vec::new();
        let mut max_id: Option<i64> = None;

        for update in updates {
            // Track the highest update_id we have seen.
            max_id = Some(max_id.map_or(update.update_id, |m| m.max(update.update_id)));

            let message = match update.message {
                Some(m) => m,
                None => continue, // skip non-message updates (edited, channel_post, etc.)
            };

            // Filter by allowed chat IDs if configured.
            if !self.config.allowed_chat_ids.is_empty()
                && !self.config.allowed_chat_ids.contains(&message.chat.id)
            {
                tracing::debug!(
                    chat_id = message.chat.id,
                    "Skipping message from non-allowed chat"
                );
                continue;
            }

            // Extract text content: prefer `text`, fall back to `caption`.
            let text = match message.text.as_deref().or(message.caption.as_deref()) {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => continue, // skip updates with no textual content
            };

            // Build sender info.
            let sender = message.from.as_ref().map(|user| {
                let display = if let Some(ref uname) = user.username {
                    format!("{} (@{})", user.first_name, uname)
                } else {
                    user.first_name.clone()
                };
                SenderInfo {
                    identifier: user
                        .username
                        .clone()
                        .unwrap_or_else(|| user.id.to_string()),
                    contact_id: None,
                    display_name: Some(display),
                }
            });

            // Determine risk based on sender vs owner.
            let source_risk = if self.is_owner(&message) {
                RiskLevel::Safe // L1
            } else {
                RiskLevel::Caution // L2
            };

            let timestamp: DateTime<Utc> = Utc
                .timestamp_opt(message.date, 0)
                .single()
                .unwrap_or_else(Utc::now);

            let summary = if text.len() > 100 {
                format!("{}...", &text[..97])
            } else {
                text.clone()
            };

            let body = serde_json::json!({
                "text": text,
                "chat_id": message.chat.id,
                "chat_type": message.chat.chat_type,
                "message_id": message.message_id,
            });

            events.push(SenseEvent {
                id: Uuid::new_v4(),
                timestamp,
                source: EventSource::Messaging,
                kind: EventKind::NewMessage,
                sender,
                content: NormalizedContent {
                    summary: Some(summary),
                    body,
                    attachments: vec![],
                },
                source_risk,
                raw_id: Some(format!("telegram-{}", message.message_id)),
            });
        }

        // Persist max update_id for offset tracking.
        if let Some(max) = max_id {
            let mut guard = self.last_update_id.lock().unwrap();
            *guard = Some(max);
        }

        events
    }

    /// Return `true` if the message sender matches the configured owner.
    fn is_owner(&self, message: &TelegramMessage) -> bool {
        match (self.config.owner_user_id, &message.from) {
            (Some(owner_id), Some(user)) => user.id == owner_id,
            _ => false,
        }
    }
}

#[async_trait]
impl SenseMonitor for TelegramMonitor {
    fn sense_id(&self) -> &str {
        "telegram"
    }

    async fn init(&mut self, _config: &AthenConfig) -> Result<()> {
        if !self.config.enabled {
            tracing::info!("TelegramMonitor disabled");
            return Ok(());
        }

        if self.config.bot_token.is_empty() {
            return Err(AthenError::Config(
                "Telegram bot_token is empty".to_string(),
            ));
        }

        // Validate the token by calling getMe.
        let url = self.api_url("getMe");
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe request failed: {e}")))?;

        let body: TelegramResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getMe parse failed: {e}")))?;

        if !body.ok {
            return Err(AthenError::Config(format!(
                "Telegram getMe failed: {}",
                body.description.unwrap_or_default()
            )));
        }

        if let Some(result) = body.result {
            let username = result
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::info!(bot_username = %username, "TelegramMonitor initialized");
        }

        Ok(())
    }

    async fn poll(&self) -> Result<Vec<SenseEvent>> {
        if !self.config.enabled || self.config.bot_token.is_empty() {
            return Ok(Vec::new());
        }

        let offset = {
            let guard = self.last_update_id.lock().unwrap();
            guard.map(|id| id + 1)
        };

        let mut url = self.api_url("getUpdates");
        url.push_str("?timeout=0");
        if let Some(off) = offset {
            url.push_str(&format!("&offset={off}"));
        }

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getUpdates failed: {e}")))?;

        let body: TelegramResponse<Vec<TelegramUpdate>> = resp
            .json()
            .await
            .map_err(|e| AthenError::Other(format!("Telegram getUpdates parse failed: {e}")))?;

        if !body.ok {
            return Err(AthenError::Other(format!(
                "Telegram getUpdates error: {}",
                body.description.unwrap_or_default()
            )));
        }

        let updates = body.result.unwrap_or_default();

        if !updates.is_empty() {
            tracing::debug!(count = updates.len(), "Received Telegram updates");
        }

        Ok(self.process_updates(updates))
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.config.poll_interval_secs)
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("TelegramMonitor shutting down");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a default disabled config for testing.
    fn test_config() -> TelegramConfig {
        TelegramConfig {
            enabled: true,
            bot_token: "123456:ABC-DEF".to_string(),
            owner_user_id: Some(42),
            allowed_chat_ids: vec![],
            poll_interval_secs: 5,
        }
    }

    /// Helper: build a TelegramUpdate with a text message.
    fn make_text_update(
        update_id: i64,
        message_id: i64,
        user_id: i64,
        first_name: &str,
        username: Option<&str>,
        chat_id: i64,
        text: &str,
    ) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: Some(TelegramMessage {
                message_id,
                from: Some(TelegramUser {
                    id: user_id,
                    first_name: first_name.to_string(),
                    username: username.map(|s| s.to_string()),
                }),
                chat: TelegramChat {
                    id: chat_id,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: Some(text.to_string()),
                caption: None,
            }),
        }
    }

    // ---------------------------------------------------------------
    // Basic properties
    // ---------------------------------------------------------------

    #[test]
    fn construction_with_config() {
        let config = test_config();
        let monitor = TelegramMonitor::new(config.clone());
        assert_eq!(monitor.config.bot_token, "123456:ABC-DEF");
        assert_eq!(monitor.config.owner_user_id, Some(42));
        assert!(monitor.last_update_id.lock().unwrap().is_none());
    }

    #[test]
    fn sense_id_is_telegram() {
        let monitor = TelegramMonitor::new(test_config());
        assert_eq!(monitor.sense_id(), "telegram");
    }

    #[test]
    fn poll_interval_from_config() {
        let mut config = test_config();
        config.poll_interval_secs = 10;
        let monitor = TelegramMonitor::new(config);
        assert_eq!(monitor.poll_interval(), Duration::from_secs(10));
    }

    #[test]
    fn default_poll_interval_is_5s() {
        let config = TelegramConfig::default();
        assert_eq!(config.poll_interval_secs, 5);
    }

    // ---------------------------------------------------------------
    // JSON deserialization of Telegram API responses
    // ---------------------------------------------------------------

    #[test]
    fn parse_valid_get_updates_response() {
        let json = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 100,
                    "message": {
                        "message_id": 1,
                        "from": { "id": 42, "first_name": "Alex", "username": "alexdev" },
                        "chat": { "id": 42, "type": "private" },
                        "date": 1700000000,
                        "text": "Hello bot!"
                    }
                }
            ]
        }"#;

        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        assert!(resp.ok);
        let updates = resp.result.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 100);
        let msg = updates[0].message.as_ref().unwrap();
        assert_eq!(msg.text.as_deref(), Some("Hello bot!"));
        assert_eq!(msg.from.as_ref().unwrap().first_name, "Alex");
        assert_eq!(
            msg.from.as_ref().unwrap().username.as_deref(),
            Some("alexdev")
        );
    }

    #[test]
    fn parse_response_with_no_messages() {
        let json = r#"{ "ok": true, "result": [] }"#;
        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        assert!(resp.ok);
        assert!(resp.result.unwrap().is_empty());
    }

    #[test]
    fn parse_response_with_photo_caption() {
        let json = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 200,
                    "message": {
                        "message_id": 5,
                        "from": { "id": 99, "first_name": "Bob" },
                        "chat": { "id": 99, "type": "private" },
                        "date": 1700000000,
                        "caption": "Check out this photo!"
                    }
                }
            ]
        }"#;

        let resp: TelegramResponse<Vec<TelegramUpdate>> =
            serde_json::from_str(json).expect("parse failed");
        let updates = resp.result.unwrap();
        let msg = updates[0].message.as_ref().unwrap();
        assert!(msg.text.is_none());
        assert_eq!(msg.caption.as_deref(), Some("Check out this photo!"));
    }

    // ---------------------------------------------------------------
    // process_updates logic
    // ---------------------------------------------------------------

    #[test]
    fn process_updates_converts_text_message() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![make_text_update(
            100, 1, 99, "Bob", Some("bob123"), 99, "Hello!",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);

        let event = &events[0];
        assert_eq!(event.source, EventSource::Messaging);
        assert!(matches!(event.kind, EventKind::NewMessage));
        assert_eq!(event.raw_id.as_deref(), Some("telegram-1"));
        assert_eq!(event.content.summary.as_deref(), Some("Hello!"));
        assert_eq!(event.content.body["text"], "Hello!");
        assert_eq!(event.content.body["chat_id"], 99);

        let sender = event.sender.as_ref().unwrap();
        assert_eq!(sender.identifier, "bob123");
        assert_eq!(
            sender.display_name.as_deref(),
            Some("Bob (@bob123)")
        );
    }

    #[test]
    fn process_updates_caption_fallback() {
        let monitor = TelegramMonitor::new(test_config());
        let update = TelegramUpdate {
            update_id: 200,
            message: Some(TelegramMessage {
                message_id: 5,
                from: Some(TelegramUser {
                    id: 99,
                    first_name: "Carol".to_string(),
                    username: None,
                }),
                chat: TelegramChat {
                    id: 99,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: None,
                caption: Some("Photo caption".to_string()),
            }),
        };

        let events = monitor.process_updates(vec![update]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("Photo caption"));
        // Sender without username uses user ID as identifier.
        let sender = events[0].sender.as_ref().unwrap();
        assert_eq!(sender.identifier, "99");
        assert_eq!(sender.display_name.as_deref(), Some("Carol"));
    }

    #[test]
    fn process_updates_owner_gets_l1_risk() {
        let monitor = TelegramMonitor::new(test_config());
        // owner_user_id is 42
        let updates = vec![make_text_update(
            100, 1, 42, "Alex", Some("alexdev"), 42, "Owner message",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Safe); // L1
    }

    #[test]
    fn process_updates_non_owner_gets_l2_risk() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![make_text_update(
            100, 1, 999, "Stranger", None, 999, "Hi there",
        )];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_risk, RiskLevel::Caution); // L2
    }

    #[test]
    fn process_updates_filters_by_allowed_chat_ids() {
        let mut config = test_config();
        config.allowed_chat_ids = vec![100, 200];
        let monitor = TelegramMonitor::new(config);

        let updates = vec![
            make_text_update(1, 1, 42, "Alex", None, 100, "Allowed chat"),
            make_text_update(2, 2, 42, "Alex", None, 300, "Blocked chat"),
            make_text_update(3, 3, 42, "Alex", None, 200, "Another allowed"),
        ];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content.body["chat_id"], 100);
        assert_eq!(events[1].content.body["chat_id"], 200);
    }

    #[test]
    fn process_updates_skips_updates_without_message() {
        let monitor = TelegramMonitor::new(test_config());
        let updates = vec![
            TelegramUpdate {
                update_id: 1,
                message: None,
            },
            make_text_update(2, 10, 42, "Alex", None, 42, "Real message"),
        ];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content.summary.as_deref(), Some("Real message"));
    }

    #[test]
    fn process_updates_skips_empty_text_and_caption() {
        let monitor = TelegramMonitor::new(test_config());
        let update = TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                from: Some(TelegramUser {
                    id: 42,
                    first_name: "Alex".to_string(),
                    username: None,
                }),
                chat: TelegramChat {
                    id: 42,
                    chat_type: "private".to_string(),
                },
                date: 1700000000,
                text: None,
                caption: None,
            }),
        };

        let events = monitor.process_updates(vec![update]);
        assert!(events.is_empty());
    }

    #[test]
    fn process_updates_tracks_last_update_id() {
        let monitor = TelegramMonitor::new(test_config());
        assert!(monitor.last_update_id.lock().unwrap().is_none());

        let updates = vec![
            make_text_update(10, 1, 42, "A", None, 42, "msg1"),
            make_text_update(15, 2, 42, "A", None, 42, "msg2"),
            make_text_update(12, 3, 42, "A", None, 42, "msg3"),
        ];

        monitor.process_updates(updates);
        assert_eq!(*monitor.last_update_id.lock().unwrap(), Some(15));
    }

    #[test]
    fn process_updates_long_text_truncated_in_summary() {
        let monitor = TelegramMonitor::new(test_config());
        let long_text = "a".repeat(200);
        let updates = vec![make_text_update(1, 1, 42, "A", None, 42, &long_text)];

        let events = monitor.process_updates(updates);
        assert_eq!(events.len(), 1);
        let summary = events[0].content.summary.as_ref().unwrap();
        assert_eq!(summary.len(), 100); // 97 chars + "..."
        assert!(summary.ends_with("..."));
        // Full text is in body.
        assert_eq!(events[0].content.body["text"].as_str().unwrap().len(), 200);
    }

    // ---------------------------------------------------------------
    // SenseMonitor trait: poll returns empty when disabled
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn poll_returns_empty_when_disabled() {
        let mut config = test_config();
        config.enabled = false;
        let monitor = TelegramMonitor::new(config);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_returns_empty_when_token_empty() {
        let mut config = test_config();
        config.bot_token = String::new();
        let monitor = TelegramMonitor::new(config);
        let events = monitor.poll().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn shutdown_succeeds() {
        let monitor = TelegramMonitor::new(test_config());
        monitor.shutdown().await.unwrap();
    }

    // ---------------------------------------------------------------
    // Config defaults
    // ---------------------------------------------------------------

    #[test]
    fn telegram_config_default_is_disabled() {
        let config = TelegramConfig::default();
        assert!(!config.enabled);
        assert!(config.bot_token.is_empty());
        assert!(config.owner_user_id.is_none());
        assert!(config.allowed_chat_ids.is_empty());
        assert_eq!(config.poll_interval_secs, 5);
    }

    #[test]
    fn telegram_config_deserializes_from_empty_toml() {
        let toml_str = "";
        let config: TelegramConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.poll_interval_secs, 5);
    }

    #[test]
    fn telegram_config_deserializes_partial() {
        let toml_str = r#"
            enabled = true
            bot_token = "123:ABC"
            owner_user_id = 42
        "#;
        let config: TelegramConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.bot_token, "123:ABC");
        assert_eq!(config.owner_user_id, Some(42));
        assert!(config.allowed_chat_ids.is_empty());
        assert_eq!(config.poll_interval_secs, 5);
    }
}
