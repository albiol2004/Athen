use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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

/// Stable identifier for an attachment across the AttachmentRef lifecycle:
/// from sense fetch, through agent tool calls, past TTL purge, into refetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttachmentId(pub Uuid);

impl AttachmentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AttachmentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Attachment metadata + a refetch-capable source pointer. After the TTL
/// purger nulls `local_path`, the agent can still read the cached
/// `extracted_text_path` (PDF text sidecar) or re-download via `source`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Where the bytes live on disk. `None` after TTL purge, or for
    /// metadata-only attachments that policy refused to download.
    pub local_path: Option<PathBuf>,
    /// Cached extracted text (e.g. PDF → .txt sidecar). Outlives
    /// `local_path` so arc continuity survives byte purge.
    pub extracted_text_path: Option<PathBuf>,
    /// Pointer back to the originating sense — lets us refetch after
    /// the bytes have been purged. `None` for synthesised/test events.
    pub source: Option<AttachmentSource>,
    pub fetched_at: DateTime<Utc>,
    pub purged_at: Option<DateTime<Utc>>,
}

/// Per-sense coordinates that uniquely identify an attachment on its
/// origin server. Stored alongside each `Attachment` so we can re-pull
/// the bytes on demand after TTL purge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentSource {
    /// IMAP coordinates. `uid_validity` is captured at fetch time so we
    /// can detect mailbox renumber (rare but real) and surface a clean
    /// "no longer fetchable" instead of grabbing the wrong message.
    Email {
        account_id: String,
        mailbox: String,
        uid_validity: u32,
        uid: u32,
        /// IMAP body part path, e.g. `"2.1"` for `BODY[2.1]`. Lets us
        /// re-fetch just the attachment instead of the whole message.
        part_path: String,
    },
    /// Telegram blob handle. `getFile(file_id)` returns a `file_path`
    /// that we then GET to re-download the bytes. Practically eternal
    /// per-bot but not formally guaranteed by Telegram — handle 404s.
    Telegram {
        chat_id: i64,
        message_id: i64,
        file_id: String,
    },
}

impl Attachment {
    /// Construct a fresh attachment record at fetch time. `local_path`
    /// is `Some` when bytes were saved; `None` for metadata-only refs
    /// (sender trust too low, MIME blocked, size cap exceeded).
    pub fn new(
        name: impl Into<String>,
        mime_type: impl Into<String>,
        size_bytes: u64,
        local_path: Option<PathBuf>,
        source: Option<AttachmentSource>,
    ) -> Self {
        Self {
            id: AttachmentId::new(),
            name: name.into(),
            mime_type: mime_type.into(),
            size_bytes,
            local_path,
            extracted_text_path: None,
            source,
            fetched_at: Utc::now(),
            purged_at: None,
        }
    }

    /// True once the bytes have been deleted by the TTL purger.
    pub fn is_purged(&self) -> bool {
        self.purged_at.is_some()
    }

    /// True if the bytes are currently readable on disk.
    pub fn is_local(&self) -> bool {
        self.local_path.is_some() && self.purged_at.is_none()
    }
}
