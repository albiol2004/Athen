//! Approval primitives shared across crates.
//!
//! An [`ApprovalQuestion`] is a request asked of the user that must be
//! answered with one of a fixed set of [`ApprovalChoice`]s. Sinks
//! ([`crate::traits::approval::ApprovalSink`]) deliver the question via
//! a specific channel (in-app, Telegram, etc.) and resolve when the
//! user picks a choice.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::notification::{NotificationOrigin, NotificationUrgency};
use crate::task::TaskId;

/// A reply channel that can both deliver messages to the user and surface
/// the user's response back to the agent.
///
/// Mirrors [`crate::config::NotificationChannelKind`] but is specific to
/// the approval-routing layer so the two concerns evolve independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplyChannelKind {
    InApp,
    Telegram,
}

impl ReplyChannelKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::InApp => "in_app",
            Self::Telegram => "telegram",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "in_app" | "inapp" => Some(Self::InApp),
            "telegram" => Some(Self::Telegram),
            _ => None,
        }
    }
}

/// Semantic intent of a single answer choice. Lets sinks render a
/// universal "approve/deny" affordance even when labels differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalChoiceKind {
    Approve,
    Deny,
    AllowOnce,
    AllowAlways,
    Cancel,
    Custom,
}

/// A single button/option in an approval question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalChoice {
    /// Stable identifier returned in the answer (e.g. "approve", "deny").
    pub key: String,
    /// Human-facing label shown on the button.
    pub label: String,
    /// Semantic role of the choice for sinks that need to style them.
    pub kind: ApprovalChoiceKind,
}

impl ApprovalChoice {
    pub fn approve() -> Self {
        Self {
            key: "approve".into(),
            label: "Approve".into(),
            kind: ApprovalChoiceKind::Approve,
        }
    }

    pub fn deny() -> Self {
        Self {
            key: "deny".into(),
            label: "Deny".into(),
            kind: ApprovalChoiceKind::Deny,
        }
    }
}

/// A request for a decision from the user.
///
/// `arc_id` and `task_id` link the question to the conversation/task it
/// originates from so callers can correlate the answer with the work
/// awaiting approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalQuestion {
    pub id: Uuid,
    pub prompt: String,
    pub description: Option<String>,
    pub choices: Vec<ApprovalChoice>,
    pub arc_id: Option<String>,
    pub task_id: Option<TaskId>,
    pub origin: NotificationOrigin,
    pub urgency: NotificationUrgency,
    pub created_at: DateTime<Utc>,
}

impl ApprovalQuestion {
    /// Build a binary approve/deny question.
    pub fn approve_or_deny(prompt: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            prompt: prompt.into(),
            description: None,
            choices: vec![ApprovalChoice::approve(), ApprovalChoice::deny()],
            arc_id: None,
            task_id: None,
            origin: NotificationOrigin::RiskSystem,
            urgency: NotificationUrgency::High,
            created_at: Utc::now(),
        }
    }
}

/// The user's response to an [`ApprovalQuestion`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalAnswer {
    pub question_id: Uuid,
    pub choice_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_channel_kind_roundtrips_strings() {
        for kind in [ReplyChannelKind::InApp, ReplyChannelKind::Telegram] {
            let s = kind.as_str();
            assert_eq!(ReplyChannelKind::from_str(s), Some(kind));
        }
        assert_eq!(ReplyChannelKind::from_str("nope"), None);
        // Tolerate the alternate spelling so we can read older rows.
        assert_eq!(
            ReplyChannelKind::from_str("inapp"),
            Some(ReplyChannelKind::InApp)
        );
    }

    #[test]
    fn approve_or_deny_has_two_choices_and_correct_kinds() {
        let q = ApprovalQuestion::approve_or_deny("Send the email?");
        assert_eq!(q.choices.len(), 2);
        assert_eq!(q.choices[0].kind, ApprovalChoiceKind::Approve);
        assert_eq!(q.choices[1].kind, ApprovalChoiceKind::Deny);
        assert_eq!(q.prompt, "Send the email?");
        assert!(q.description.is_none());
    }

    #[test]
    fn approval_question_serializes_round_trip() {
        let q = ApprovalQuestion::approve_or_deny("Run shell command?");
        let json = serde_json::to_string(&q).expect("serialize");
        let back: ApprovalQuestion = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, q.id);
        assert_eq!(back.choices.len(), q.choices.len());
        assert_eq!(back.choices[0].key, "approve");
    }

    #[test]
    fn approval_answer_serializes_round_trip() {
        let q = ApprovalQuestion::approve_or_deny("Send?");
        let a = ApprovalAnswer {
            question_id: q.id,
            choice_key: "approve".into(),
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: ApprovalAnswer = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.question_id, q.id);
        assert_eq!(back.choice_key, "approve");
    }
}
