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

use athen_agent::tools::{EmailSendApprovalGate, EmailSendSummary};
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
