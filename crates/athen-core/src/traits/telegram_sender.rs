//! Outbound Telegram port. Adapters (currently `BotApiTelegramSender` in
//! athen-sentidos) implement this so the agent's `send_telegram` tool can
//! deliver messages — text and/or file attachments — without pulling the
//! Bot API client into athen-core.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// What kind of upload Telegram should treat this attachment as.
///
/// Telegram's Bot API has separate endpoints per media kind (`sendPhoto`,
/// `sendDocument`, `sendVideo`, …). Each endpoint enforces different size
/// caps and renders the file differently in the client. `Auto` lets the
/// adapter pick based on file extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramAttachmentKind {
    /// Render as an inline photo. Telegram re-compresses; lossy. Use for
    /// screenshots, charts, snapshots. Max ~10 MB.
    Photo,
    /// Render as a downloadable file with the original filename and bytes
    /// preserved. The right choice for PDFs, archives, code, anything the
    /// recipient needs intact. Max 50 MB via bot API.
    Document,
    /// Pick based on extension: image extensions → Photo, everything else →
    /// Document.
    #[default]
    Auto,
}

/// One file the agent wants to attach.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramAttachment {
    /// Absolute path on disk to the file being uploaded. The adapter
    /// reads the file at send time.
    pub path: PathBuf,
    /// How Telegram should render the file. Defaults to `Auto`.
    #[serde(default)]
    pub kind: TelegramAttachmentKind,
    /// Optional per-attachment caption (max 1024 chars in the Telegram
    /// client). When omitted and the message has a single attachment with
    /// short text, the adapter may use the message text as the caption
    /// instead of sending it as a separate bubble.
    #[serde(default)]
    pub caption: Option<String>,
}

/// One outbound Telegram message. Either `text` or at least one
/// attachment must be present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundTelegramMessage {
    /// Destination chat. When `None`, the adapter uses its configured
    /// owner-chat default (typically the Telegram user ID of the bot's
    /// owner). Tools should normally leave this unset and let the
    /// adapter fill it.
    #[serde(default)]
    pub chat_id: Option<i64>,
    /// Plain-text message body. Optional only when `attachments` is
    /// non-empty.
    #[serde(default)]
    pub text: Option<String>,
    /// Files to send. Empty means text-only.
    #[serde(default)]
    pub attachments: Vec<TelegramAttachment>,
    /// Optional `message_id` of the message we're replying to, so the
    /// outbound message threads as a reply in the client.
    #[serde(default)]
    pub reply_to_message_id: Option<i64>,
}

/// Result of a successful send. Multiple message IDs when the body was
/// split (Telegram's 4096-char text cap) or when both text and
/// attachments were sent as separate API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentTelegramMessage {
    pub message_ids: Vec<i64>,
    /// Resolved destination chat the adapter actually used (handy when
    /// the tool passed `chat_id: None` and the adapter substituted its
    /// owner-chat default).
    pub chat_id: i64,
}

#[async_trait]
pub trait TelegramSender: Send + Sync {
    /// Deliver one message. Adapters should map transport errors to
    /// `AthenError` and surface partial-success situations (e.g. text
    /// sent, attachment upload failed) as `Err` with a message clear
    /// enough that the agent can decide whether to retry.
    async fn send(&self, msg: &OutboundTelegramMessage) -> Result<SentTelegramMessage>;

    /// Cheap connectivity / auth probe used by the Settings UI's
    /// "Test Telegram" button. Returns `Ok(())` when the bot token
    /// authenticates against `getMe`. Does NOT send a message.
    async fn test_connection(&self) -> Result<()>;

    /// The owner / default chat this sender will deliver to when callers
    /// omit `chat_id`. `None` means no default is wired and the tool
    /// must supply an explicit destination.
    fn default_chat_id(&self) -> Option<i64>;

    /// Stable identifier for logs.
    fn name(&self) -> &'static str;
}
