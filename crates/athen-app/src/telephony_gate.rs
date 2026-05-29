//! Production [`TelephonyApprovalGate`] — routes the place-call
//! confirmation through Athen's cross-channel ApprovalRouter (InApp +
//! Telegram, with escalation). Fails closed on any non-approve outcome.
//!
//! Mirrors [`crate::email_gate::RouterEmailApprovalGate`].

use std::sync::Arc;

use async_trait::async_trait;
use athen_core::config::SecurityMode;
use athen_voice::{CallRequest, CalledParty, TelephonyApprovalGate};
use uuid::Uuid;

pub struct RouterTelephonyApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
    /// Effective security posture for this arc (per-arc override ⊕ live
    /// global). Under `Yolo`, `confirm_call` short-circuits to approved
    /// without prompting. Defaults to `Assistant` (today's
    /// always-prompt behaviour). Mirrors
    /// [`crate::file_gate::FileGate::with_security_mode`].
    security_mode: SecurityMode,
}

impl RouterTelephonyApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self {
            router,
            arc_id,
            security_mode: SecurityMode::Assistant,
        }
    }

    /// Set the effective security posture for this arc. Under `Yolo`,
    /// `confirm_call` returns `true` without prompting; `Assistant` /
    /// `Bunker` keep today's router round-trip.
    pub fn with_security_mode(mut self, mode: SecurityMode) -> Self {
        self.security_mode = mode;
        self
    }

    /// Compose the approval-dialog description from a [`CallRequest`].
    /// Pulled out so the unit test can exercise the formatter without
    /// spinning up an ApprovalRouter.
    pub(crate) fn build_description(request: &CallRequest) -> String {
        let party_label = match request.called_party {
            CalledParty::User => "you (reminder call)",
            CalledParty::Other => "external",
        };
        let voice_line = request
            .voice_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(default)");

        format!(
            "To: {to} ({party})\n\nObjective: {obj}\n\nEstimated cost: ~${cost:.2} (up to {dur}s)\nVoice: {voice}\nLLM: {llm}\nStack: STT {stt} / TTS {tts} / Phone {phone}",
            to = request.to_number,
            party = party_label,
            obj = request.objective,
            cost = request.est_cost_usd,
            dur = request.max_duration_s,
            voice = voice_line,
            llm = request.llm_label,
            stt = request.stt_provider,
            tts = request.voice_provider,
            phone = request.phone_provider,
        )
    }
}

#[async_trait]
impl TelephonyApprovalGate for RouterTelephonyApprovalGate {
    async fn confirm_call(&self, request: &CallRequest) -> bool {
        use athen_core::approval::{ApprovalChoice, ApprovalQuestion};
        use athen_core::notification::{NotificationOrigin, NotificationUrgency};

        // Yolo loosens only: the call proceeds silently without a prompt.
        // place_call's own destination/cost guards remain upstream.
        if self.security_mode == SecurityMode::Yolo {
            tracing::debug!(
                arc = ?self.arc_id,
                to = %request.to_number,
                "place_call approval auto-approved under Yolo (no prompt)"
            );
            return true;
        }

        let prompt = format!("Place phone call to {}?", request.to_number);
        let desc = Self::build_description(request);

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
                    "place_call approval router failed; treating as deny"
                );
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_voice::estimate_call_cost_usd;

    fn sample_request(called_party: CalledParty) -> CallRequest {
        CallRequest {
            arc_id: Uuid::new_v4(),
            to_number: "+14155551234".into(),
            objective: "Book a table for 4 at 8pm".into(),
            called_party,
            voice_id: Some("rachel".into()),
            max_duration_s: 600,
            est_cost_usd: estimate_call_cost_usd(600),
            llm_label: "DeepSeek :: deepseek-chat".into(),
            voice_provider: "ElevenLabs".into(),
            stt_provider: "Deepgram".into(),
            phone_provider: "Twilio".into(),
        }
    }

    #[test]
    fn description_includes_every_decision_field_for_external_calls() {
        let req = sample_request(CalledParty::Other);
        let desc = RouterTelephonyApprovalGate::build_description(&req);
        assert!(desc.contains("+14155551234"));
        assert!(desc.contains("external"));
        assert!(desc.contains("Book a table"));
        assert!(desc.contains("$0.91"));
        assert!(desc.contains("up to 600s"));
        assert!(desc.contains("rachel"));
        assert!(desc.contains("DeepSeek :: deepseek-chat"));
        assert!(desc.contains("Deepgram"));
        assert!(desc.contains("ElevenLabs"));
        assert!(desc.contains("Twilio"));
    }

    #[test]
    fn description_labels_user_party_distinctly() {
        let req = sample_request(CalledParty::User);
        let desc = RouterTelephonyApprovalGate::build_description(&req);
        assert!(
            desc.contains("you (reminder call)"),
            "expected user-party label, got:\n{desc}"
        );
    }

    #[test]
    fn description_handles_missing_voice_id() {
        let mut req = sample_request(CalledParty::Other);
        req.voice_id = None;
        let desc = RouterTelephonyApprovalGate::build_description(&req);
        assert!(desc.contains("Voice: (default)"));
    }

    /// A router with no sinks: `ask_with_escalation` fails immediately
    /// (no sink for the primary channel), so the non-Yolo path returns
    /// `false` (deny) WITHOUT prompting/hanging.
    fn sinkless_router() -> Arc<crate::approval::ApprovalRouter> {
        Arc::new(crate::approval::ApprovalRouter::new(vec![]))
    }

    #[test]
    fn gate_defaults_to_assistant_mode() {
        let gate = RouterTelephonyApprovalGate::new(sinkless_router(), None);
        assert_eq!(gate.security_mode, SecurityMode::Assistant);
    }

    #[tokio::test]
    async fn yolo_approves_call_without_prompting() {
        // Yolo short-circuits to approved without touching the router.
        let gate = RouterTelephonyApprovalGate::new(sinkless_router(), Some("arc_x".into()))
            .with_security_mode(SecurityMode::Yolo);
        assert!(gate.confirm_call(&sample_request(CalledParty::Other)).await);
    }

    #[tokio::test]
    async fn assistant_and_bunker_still_route() {
        // Non-Yolo modes consult the router; with no sinks it fails closed
        // to deny (`false`), proving no auto-approve short-circuit fired.
        for mode in [SecurityMode::Assistant, SecurityMode::Bunker] {
            let gate = RouterTelephonyApprovalGate::new(sinkless_router(), Some("arc_x".into()))
                .with_security_mode(mode);
            assert!(
                !gate.confirm_call(&sample_request(CalledParty::Other)).await,
                "mode {mode:?} should route (and fail closed to deny), not auto-approve"
            );
        }
    }
}
