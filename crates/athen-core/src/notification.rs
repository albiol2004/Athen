use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::NotificationChannelKind;
use crate::task::TaskId;

/// How urgently the notification must reach the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationUrgency {
    /// Can wait, batch during quiet hours.
    Low,
    /// Should reach user soon.
    Medium,
    /// Must reach user, escalate if no response.
    High,
    /// Override quiet hours, try all channels.
    Critical,
}

/// Which subsystem originated this notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationOrigin {
    RiskSystem,
    SenseRouter,
    Agent,
    System,
}

/// A notification to be delivered to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: Uuid,
    pub urgency: NotificationUrgency,
    pub title: String,
    pub body: String,
    pub origin: NotificationOrigin,
    pub arc_id: Option<String>,
    pub task_id: Option<TaskId>,
    pub created_at: DateTime<Utc>,
    /// Whether the notification requires an explicit user response (e.g. approval).
    pub requires_response: bool,
    /// Skip the LLM "humanize" rewrite step in `NotificationOrchestrator`.
    /// Set this when the title/body are already structured assistant-voice
    /// copy (e.g. "Athen is handling email from Alex") — the rewrite prompt
    /// assumes raw event data and would otherwise paraphrase the structure
    /// away or, worse, get confused by salutations addressing Athen itself.
    #[serde(default)]
    pub skip_humanize: bool,
    /// Full long-form body for chat-style channels (Telegram, future
    /// SMS/WhatsApp). When `Some`, channels that aren't space-constrained
    /// SHOULD prefer this over `body`. `body` itself stays short for
    /// in-app toast previews. `None` = use `body` everywhere.
    #[serde(default)]
    pub body_long: Option<String>,
}

/// Result of attempting to deliver a notification through a channel.
#[derive(Debug, Clone)]
pub enum DeliveryResult {
    Delivered,
    Failed(String),
}

/// Tracking status of a notification through the delivery pipeline.
#[derive(Debug, Clone)]
pub enum DeliveryStatus {
    Pending,
    Delivered(NotificationChannelKind),
    Seen,
    Escalated(NotificationChannelKind),
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotificationConfig;

    #[test]
    fn test_notification_creation() {
        let notif = Notification {
            id: Uuid::new_v4(),
            urgency: NotificationUrgency::High,
            title: "Test alert".to_string(),
            body: "Something happened".to_string(),
            origin: NotificationOrigin::RiskSystem,
            arc_id: Some("arc-123".to_string()),
            task_id: None,
            created_at: Utc::now(),
            requires_response: true,
            skip_humanize: false,
            body_long: None,
        };

        assert_eq!(notif.urgency, NotificationUrgency::High);
        assert_eq!(notif.title, "Test alert");
        assert_eq!(notif.body, "Something happened");
        assert_eq!(notif.origin, NotificationOrigin::RiskSystem);
        assert_eq!(notif.arc_id, Some("arc-123".to_string()));
        assert!(notif.task_id.is_none());
        assert!(notif.requires_response);
        assert!(notif.body_long.is_none());
    }

    /// Old payloads from before the `body_long` field landed must still
    /// deserialize cleanly. The `#[serde(default)]` attribute is what
    /// makes that work — easy to lose to a future refactor.
    #[test]
    fn deserializes_legacy_payload_without_body_long() {
        let json = serde_json::json!({
            "id": Uuid::new_v4().to_string(),
            "urgency": "Low",
            "title": "Old payload",
            "body": "no body_long field on this row",
            "origin": "Agent",
            "arc_id": null,
            "task_id": null,
            "created_at": Utc::now().to_rfc3339(),
            "requires_response": false,
        });
        let n: Notification = serde_json::from_value(json).expect("legacy payload");
        assert!(n.body_long.is_none());
    }

    #[test]
    fn test_urgency_serialization() {
        let variants = vec![
            NotificationUrgency::Low,
            NotificationUrgency::Medium,
            NotificationUrgency::High,
            NotificationUrgency::Critical,
        ];

        for urgency in variants {
            let json = serde_json::to_string(&urgency).expect("serialize");
            let deserialized: NotificationUrgency =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(urgency, deserialized);
        }
    }

    #[test]
    fn test_notification_config_default() {
        let config = NotificationConfig::default();
        assert_eq!(
            config.preferred_channels,
            vec![
                NotificationChannelKind::InApp,
                NotificationChannelKind::Telegram
            ]
        );
        assert_eq!(config.escalation_timeout_secs, 300);
        assert!(config.quiet_hours.is_none());
    }
}
