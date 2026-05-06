//! Production EmailSendApprovalGate — routes the question through the
//! cross-channel ApprovalRouter so the user can answer in-app or via
//! Telegram (with escalation). Fails closed on any non-approve outcome.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use athen_agent::tools::{EmailSendApprovalGate, EmailSendSummary};

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
