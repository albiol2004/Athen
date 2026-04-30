//! Trait contract for delivering approval requests to the user.

use async_trait::async_trait;
use uuid::Uuid;

use crate::approval::{ApprovalAnswer, ApprovalQuestion, ReplyChannelKind};
use crate::error::Result;

/// A single channel through which an approval question can be asked.
///
/// `ask` returns the user's chosen answer. Implementations are expected
/// to manage their own delivery + waiting (e.g. an in-app sink stores a
/// oneshot keyed by `question.id`; a Telegram sink sends an inline
/// keyboard and resolves on the corresponding `callback_query`).
///
/// Sinks are racing-friendly: an [`ApprovalRouter`] may call `ask` on
/// several sinks in parallel and `cancel` whichever didn't win.
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    /// Which channel this sink represents.
    fn channel_kind(&self) -> ReplyChannelKind;

    /// Deliver the question and await the user's choice.
    async fn ask(&self, question: ApprovalQuestion) -> Result<ApprovalAnswer>;

    /// Cancel a pending question (e.g. another sink already won the race).
    /// Implementations should drop any oneshot waiters and stop polling
    /// for an answer to this question_id. Default impl is a no-op for
    /// sinks that don't park resources per question.
    async fn cancel(&self, _question_id: Uuid) -> Result<()> {
        Ok(())
    }
}
