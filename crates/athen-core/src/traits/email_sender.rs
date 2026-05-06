//! Outbound email port. Adapters (currently `LettreSmtpSender` in athen-sentidos)
//! implement this so the agent's `email_send` tool can dispatch mail without
//! pulling SMTP libraries into athen-core.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One outbound email. `to` is required and must be non-empty.
/// `cc`/`bcc` may be empty. `body_html` is optional — when set, the message
/// is sent as multipart/alternative with `body_text` as the plain fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundEmail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    /// When `Some`, sets the `In-Reply-To` and `References` headers so the
    /// message threads in the recipient's client.
    pub in_reply_to: Option<String>,
}

/// Result of a successful send. The message-id is what Athen will store
/// so a subsequent reply can be threaded against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentEmail {
    pub message_id: String,
    pub accepted_recipients: Vec<String>,
}

#[async_trait]
pub trait EmailSender: Send + Sync {
    /// Send one message. Adapters should map transport errors to AthenError.
    async fn send(&self, email: &OutboundEmail) -> Result<SentEmail>;

    /// Cheap connectivity / auth probe used by the Settings UI's
    /// "Test SMTP" button. Returning `Ok(())` means the credentials
    /// authenticate; it does NOT send a message.
    async fn test_connection(&self) -> Result<()>;

    /// Stable identifier for logs.
    fn name(&self) -> &'static str;
}
