//! Production EmailSendApprovalGate — routes the question through the
//! cross-channel ApprovalRouter so the user can answer in-app or via
//! Telegram (with escalation). Fails closed on any non-approve outcome.
//!
//! Also hosts the [`OwnerLookupAdapter`] that bridges
//! `athen_contacts::OwnerLookup` to the agent-side
//! [`athen_agent::OwnerDestinationCheck`] trait — keeps the dep graph
//! clean (the agent crate never has to depend on athen-contacts).

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use athen_agent::tools::{
    EmailSendApprovalGate, EmailSendSummary, TelegramOutboundRecorder, TelegramSendApprovalGate,
    TelegramSendSummary,
};
use athen_agent::OwnerDestinationCheck;
use athen_contacts::OwnerLookup;

pub struct RouterEmailApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
}

impl RouterEmailApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self { router, arc_id }
    }
}

#[async_trait]
impl EmailSendApprovalGate for RouterEmailApprovalGate {
    async fn confirm_send(&self, summary: &EmailSendSummary) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        let to_line = summary.to.join(", ");
        let prompt = format!("Send email to {to_line}?");
        // Body preview + cc/bcc lines for the description, so the user has
        // enough context to decide without opening another screen.
        let mut desc = format!("Subject: {}\n\n{}", summary.subject, summary.body_preview);
        if !summary.cc.is_empty() {
            desc.push_str(&format!("\n\nCc: {}", summary.cc.join(", ")));
        }
        if !summary.bcc.is_empty() {
            desc.push_str(&format!("\n\nBcc: {}", summary.bcc.join(", ")));
        }
        if summary.in_reply_to.is_some() {
            desc.push_str("\n\n(Reply to existing thread)");
        }

        let question = ApprovalQuestion {
            id: Uuid::new_v4(),
            prompt,
            description: Some(desc),
            choices: vec![ApprovalChoice::approve(), ApprovalChoice::deny()],
            arc_id: self.arc_id.clone(),
            task_id: None,
            origin: NotificationOrigin::RiskSystem,
            urgency: NotificationUrgency::High,
            created_at: chrono::Utc::now(),
        };
        let primary = self.router.pick_primary(self.arc_id.as_deref()).await;
        match self.router.ask_with_escalation(question, primary).await {
            Ok(answer) => answer.choice_key == "approve",
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "email send approval router failed; treating as deny"
                );
                false
            }
        }
    }
}

/// Production [`TelegramSendApprovalGate`] — same routing strategy as
/// the email gate: surface the question through the cross-channel
/// ApprovalRouter (in-app + Telegram with escalation) and fail closed.
/// The agent layer already bypasses the gate for owner-chat sends, so
/// this only fires for non-owner destinations.
pub struct RouterTelegramApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
}

impl RouterTelegramApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self { router, arc_id }
    }
}

#[async_trait]
impl TelegramSendApprovalGate for RouterTelegramApprovalGate {
    async fn confirm_send(&self, summary: &TelegramSendSummary) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        let att_count = summary.attachment_paths.len();
        let prompt = if att_count == 0 {
            format!("Send Telegram to chat {}?", summary.chat_id)
        } else {
            format!(
                "Send Telegram to chat {} with {} attachment{}?",
                summary.chat_id,
                att_count,
                if att_count == 1 { "" } else { "s" }
            )
        };

        let mut desc = String::new();
        if !summary.text_preview.is_empty() {
            desc.push_str(&summary.text_preview);
        }
        if att_count > 0 {
            if !desc.is_empty() {
                desc.push_str("\n\n");
            }
            desc.push_str("Attachments:\n");
            for (path, kind) in summary
                .attachment_paths
                .iter()
                .zip(summary.attachment_kinds.iter())
            {
                let label = match kind {
                    athen_core::traits::telegram_sender::TelegramAttachmentKind::Photo => "photo",
                    athen_core::traits::telegram_sender::TelegramAttachmentKind::Document => {
                        "document"
                    }
                    athen_core::traits::telegram_sender::TelegramAttachmentKind::Auto => "auto",
                };
                desc.push_str(&format!("  • {} ({label})\n", path.display()));
            }
        }
        if desc.is_empty() {
            desc.push_str("(no preview)");
        }

        let question = ApprovalQuestion {
            id: Uuid::new_v4(),
            prompt,
            description: Some(desc),
            choices: vec![ApprovalChoice::approve(), ApprovalChoice::deny()],
            arc_id: self.arc_id.clone(),
            task_id: None,
            origin: NotificationOrigin::RiskSystem,
            urgency: NotificationUrgency::High,
            created_at: chrono::Utc::now(),
        };
        let primary = self.router.pick_primary(self.arc_id.as_deref()).await;
        match self.router.ask_with_escalation(question, primary).await {
            Ok(answer) => answer.choice_key == "approve",
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "telegram send approval router failed; treating as deny"
                );
                false
            }
        }
    }
}

/// Production [`TelegramOutboundRecorder`]: stamps the in-process
/// `telegram_outbound_hint` slot with the registry's arc id after every
/// successful agent-driven Telegram send. The owner-Telegram handler
/// reads that slot when picking which arc the user's next Telegram
/// reply belongs to, so a UI-driven arc that asked Athen to "reply on
/// Telegram" stays sticky even though Telegram is technically a
/// different channel.
///
/// When `arc_id` is `None` (e.g. registry built outside of an arc
/// context like `refresh_tools_doc`), `record` is a no-op — there's
/// nothing meaningful to stamp.
pub struct ArcAwareTelegramOutboundRecorder {
    hint: crate::notifier::TelegramOutboundHint,
    arc_id: Option<String>,
}

impl ArcAwareTelegramOutboundRecorder {
    pub fn new(hint: crate::notifier::TelegramOutboundHint, arc_id: Option<String>) -> Self {
        Self { hint, arc_id }
    }
}

#[async_trait]
impl TelegramOutboundRecorder for ArcAwareTelegramOutboundRecorder {
    async fn record(&self, _chat_id: i64) {
        let Some(arc_id) = self.arc_id.as_deref() else {
            return;
        };
        crate::notifier::stamp_outbound_hint(&self.hint, arc_id);
        tracing::info!(
            arc = %arc_id,
            "Stamped Telegram outbound hint from send_telegram tool"
        );
    }
}

/// Adapter that lets the agent crate's `OwnerDestinationCheck`
/// trait be satisfied by an `Arc<OwnerLookup>` from athen-contacts.
/// Lives here rather than in athen-agent so the agent crate stays
/// free of an athen-contacts dependency.
pub struct OwnerLookupAdapter {
    lookup: Arc<OwnerLookup>,
}

impl OwnerLookupAdapter {
    pub fn new(lookup: Arc<OwnerLookup>) -> Self {
        Self { lookup }
    }
}

#[async_trait]
impl OwnerDestinationCheck for OwnerLookupAdapter {
    async fn is_owner_email(&self, email: &str) -> bool {
        // The OwnerLookup normalizes scheme="email" values to lowercase,
        // so passing the caller-lowercased value through is correct.
        self.lookup.is_owner_identifier("email", email).await
    }
}
