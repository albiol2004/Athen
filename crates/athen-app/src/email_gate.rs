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
    EmailSendApprovalGate, EmailSendSummary, OutboundTelegramSummary, TelegramOutboundRecorder,
    TelegramSendApprovalGate, TelegramSendSummary,
};
use athen_agent::OwnerDestinationCheck;
use athen_contacts::OwnerLookup;
use athen_core::config::SecurityMode;
use athen_persistence::telegram_chat_log::{TelegramChatLogStore, TelegramLogDirection};

pub struct RouterEmailApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
    /// Effective security posture for this arc (per-arc override ⊕ live
    /// global). Under `Yolo`, `confirm_send` short-circuits to approved
    /// without prompting or routing. Defaults to `Assistant` (today's
    /// always-prompt-for-non-owner behaviour). Mirrors
    /// `FileGate::with_security_mode`.
    security_mode: SecurityMode,
}

impl RouterEmailApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self {
            router,
            arc_id,
            security_mode: SecurityMode::Assistant,
        }
    }

    /// Set the effective security posture for this arc. Under `Yolo`,
    /// `confirm_send` returns approved without prompting; `Assistant` /
    /// `Bunker` keep today's routing behaviour. Mirrors
    /// `FileGate::with_security_mode`.
    pub fn with_security_mode(mut self, mode: SecurityMode) -> Self {
        self.security_mode = mode;
        self
    }
}

#[async_trait]
impl EmailSendApprovalGate for RouterEmailApprovalGate {
    async fn confirm_send(&self, summary: &EmailSendSummary) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        // Yolo loosens only: a send that would otherwise prompt proceeds
        // silently without any router round-trip. HardBlock-equivalent
        // refusals live upstream in the risk/coordinator gates, not here.
        if self.security_mode == SecurityMode::Yolo {
            tracing::debug!(
                arc = ?self.arc_id,
                "email send approval auto-approved under Yolo (no prompt)"
            );
            return true;
        }

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
    /// Effective security posture for this arc. Under `Yolo`,
    /// `confirm_send` short-circuits to approved without prompting.
    /// Defaults to `Assistant`. Mirrors `FileGate::with_security_mode`.
    security_mode: SecurityMode,
}

impl RouterTelegramApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self {
            router,
            arc_id,
            security_mode: SecurityMode::Assistant,
        }
    }

    /// Set the effective security posture for this arc. Under `Yolo`,
    /// `confirm_send` returns approved without prompting. Mirrors
    /// `FileGate::with_security_mode`.
    pub fn with_security_mode(mut self, mode: SecurityMode) -> Self {
        self.security_mode = mode;
        self
    }
}

#[async_trait]
impl TelegramSendApprovalGate for RouterTelegramApprovalGate {
    async fn confirm_send(&self, summary: &TelegramSendSummary) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        // Yolo loosens only: skip the prompt and proceed silently. The
        // owner-chat fast-path already bypasses this gate upstream, so
        // this short-circuit only relaxes the non-owner prompt.
        if self.security_mode == SecurityMode::Yolo {
            tracing::debug!(
                arc = ?self.arc_id,
                "telegram send approval auto-approved under Yolo (no prompt)"
            );
            return true;
        }

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

/// Production [`TelegramOutboundRecorder`] that does two things after
/// every successful agent-driven `send_telegram`:
///
/// 1. Stamps the in-process `telegram_outbound_hint` slot with the
///    registry's `arc_id` so the user's next Telegram reply gets
///    routed back to this arc instead of being re-triaged. When
///    `arc_id` is `None`, this step is skipped.
/// 2. Appends an outbound row to the per-`chat_id` transcript store
///    (`telegram_chat_log`) so the owner-Telegram handler can prepend
///    recent turns as system context on the next inbound — giving the
///    agent continuity even when arc routing picks the wrong arc.
pub struct ArcAwareTelegramOutboundRecorder {
    hint: crate::notifier::TelegramOutboundHint,
    arc_id: Option<String>,
    chat_log: Option<Arc<TelegramChatLogStore>>,
}

impl ArcAwareTelegramOutboundRecorder {
    pub fn new(
        hint: crate::notifier::TelegramOutboundHint,
        arc_id: Option<String>,
        chat_log: Option<Arc<TelegramChatLogStore>>,
    ) -> Self {
        Self {
            hint,
            arc_id,
            chat_log,
        }
    }
}

#[async_trait]
impl TelegramOutboundRecorder for ArcAwareTelegramOutboundRecorder {
    async fn record(&self, summary: OutboundTelegramSummary<'_>) {
        if let Some(arc_id) = self.arc_id.as_deref() {
            crate::notifier::stamp_outbound_hint(&self.hint, arc_id);
            tracing::info!(
                arc = %arc_id,
                chat_id = summary.chat_id,
                "Stamped Telegram outbound hint from send_telegram tool"
            );
        }
        if let Some(store) = self.chat_log.as_ref() {
            let body = match (summary.text, summary.attachment_count) {
                (Some(t), 0) => t.to_string(),
                (Some(t), n) => format!("{t}\n[+{n} attachment(s)]"),
                (None, n) if n > 0 => format!("[{n} attachment(s)]"),
                _ => String::new(),
            };
            if !body.is_empty() {
                if let Err(e) = store
                    .append(
                        summary.chat_id,
                        TelegramLogDirection::Outbound,
                        &body,
                        summary.attachment_count > 0,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        chat_id = summary.chat_id,
                        "telegram_chat_log append (outbound) failed"
                    );
                }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::traits::telegram_sender::TelegramAttachmentKind;

    /// A router with no sinks: any `ask_with_escalation` fails immediately
    /// (no sink for the primary channel), so the non-Yolo path returns
    /// `false` (deny) WITHOUT prompting/hanging — which is exactly what we
    /// want to assert against (the gate didn't short-circuit to approve).
    fn sinkless_router() -> Arc<crate::approval::ApprovalRouter> {
        Arc::new(crate::approval::ApprovalRouter::new(vec![]))
    }

    fn sample_email() -> EmailSendSummary {
        EmailSendSummary {
            to: vec!["someone@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Hi".into(),
            body_preview: "body".into(),
            in_reply_to: None,
        }
    }

    fn sample_telegram() -> TelegramSendSummary {
        TelegramSendSummary {
            chat_id: 42,
            to_owner: false,
            text_preview: "hello".into(),
            attachment_paths: vec![],
            attachment_kinds: vec![],
        }
    }

    #[test]
    fn email_gate_defaults_to_assistant_mode() {
        let gate = RouterEmailApprovalGate::new(sinkless_router(), None);
        assert_eq!(gate.security_mode, SecurityMode::Assistant);
    }

    #[test]
    fn telegram_gate_defaults_to_assistant_mode() {
        let gate = RouterTelegramApprovalGate::new(sinkless_router(), None);
        assert_eq!(gate.security_mode, SecurityMode::Assistant);
    }

    #[tokio::test]
    async fn email_yolo_approves_without_prompting() {
        // Yolo short-circuits to approved without touching the router.
        // The sinkless router would return deny if consulted, so a `true`
        // here proves the prompt was skipped.
        let gate = RouterEmailApprovalGate::new(sinkless_router(), Some("arc_x".into()))
            .with_security_mode(SecurityMode::Yolo);
        assert!(gate.confirm_send(&sample_email()).await);
    }

    #[tokio::test]
    async fn email_assistant_and_bunker_still_route() {
        // Non-Yolo modes consult the router. With no sinks the router
        // fails closed to deny (`false`) — proving the gate did NOT
        // short-circuit to approve and still went through the router.
        for mode in [SecurityMode::Assistant, SecurityMode::Bunker] {
            let gate = RouterEmailApprovalGate::new(sinkless_router(), Some("arc_x".into()))
                .with_security_mode(mode);
            assert!(
                !gate.confirm_send(&sample_email()).await,
                "mode {mode:?} should route (and fail closed to deny), not auto-approve"
            );
        }
    }

    #[tokio::test]
    async fn telegram_yolo_approves_without_prompting() {
        let gate = RouterTelegramApprovalGate::new(sinkless_router(), Some("arc_x".into()))
            .with_security_mode(SecurityMode::Yolo);
        assert!(gate.confirm_send(&sample_telegram()).await);
    }

    #[tokio::test]
    async fn telegram_yolo_approves_with_attachments() {
        let mut summary = sample_telegram();
        summary.attachment_paths = vec![std::path::PathBuf::from("/tmp/a.png")];
        summary.attachment_kinds = vec![TelegramAttachmentKind::Photo];
        let gate = RouterTelegramApprovalGate::new(sinkless_router(), Some("arc_x".into()))
            .with_security_mode(SecurityMode::Yolo);
        assert!(gate.confirm_send(&summary).await);
    }

    #[tokio::test]
    async fn telegram_assistant_and_bunker_still_route() {
        for mode in [SecurityMode::Assistant, SecurityMode::Bunker] {
            let gate = RouterTelegramApprovalGate::new(sinkless_router(), Some("arc_x".into()))
                .with_security_mode(mode);
            assert!(
                !gate.confirm_send(&sample_telegram()).await,
                "mode {mode:?} should route (and fail closed to deny), not auto-approve"
            );
        }
    }
}
