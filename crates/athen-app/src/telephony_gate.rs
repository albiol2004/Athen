//! Production [`TelephonyApprovalGate`] — routes the place-call
//! confirmation through Athen's cross-channel ApprovalRouter (InApp +
//! Telegram, with escalation). Fails closed on any non-approve outcome.
//!
//! Mirrors [`crate::email_gate::RouterEmailApprovalGate`].

use std::sync::Arc;

use async_trait::async_trait;
use athen_voice::{CallRequest, CalledParty, TelephonyApprovalGate};
use uuid::Uuid;

pub struct RouterTelephonyApprovalGate {
    router: Arc<crate::approval::ApprovalRouter>,
    arc_id: Option<String>,
}

impl RouterTelephonyApprovalGate {
    pub fn new(router: Arc<crate::approval::ApprovalRouter>, arc_id: Option<String>) -> Self {
        Self { router, arc_id }
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
}
