use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::contact::ContactId;
use crate::risk::RiskLevel;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenseEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub source: EventSource,
    pub kind: EventKind,
    pub sender: Option<SenderInfo>,
    pub content: NormalizedContent,
    pub source_risk: RiskLevel,
    pub raw_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EventSource {
    Email,
    Calendar,
    Messaging,
    UserInput,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    NewMessage,
    UpdatedMessage,
    Reminder,
    Notification,
    Command,
    Alert,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderInfo {
    pub identifier: String,
    pub contact_id: Option<ContactId>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedContent {
    pub summary: Option<String>,
    pub body: serde_json::Value,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub path: Option<std::path::PathBuf>,
}
