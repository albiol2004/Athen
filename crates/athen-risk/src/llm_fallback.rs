//! LLM-assisted risk evaluation for ambiguous cases (step 2).
//!
//! When the fast regex rules cannot confidently classify an action,
//! we ask an LLM to evaluate its risk.

use athen_core::contact::TrustLevel;
use athen_core::llm::{ChatMessage, LlmRequest, LlmResponse, MessageContent, ModelProfile, Role};
use athen_core::risk::{BaseImpact, DataSensitivity, EvaluationMethod, RiskContext, RiskScore};
use athen_core::traits::llm::LlmRouter;

use crate::scorer::RiskScorer;

/// LLM-assisted risk evaluator for cases where regex rules are insufficient.
pub struct LlmRiskEvaluator {
    router: Box<dyn LlmRouter>,
    scorer: RiskScorer,
}

impl LlmRiskEvaluator {
    pub fn new(router: Box<dyn LlmRouter>) -> Self {
        Self {
            router,
            scorer: RiskScorer::new(),
        }
    }

    /// Evaluate the risk of an action description using an LLM.
    ///
    /// Uses a 10-second timeout. If the LLM call fails or times out,
    /// falls back to a conservative score (WritePersist + PersonalInfo + 0.3 confidence)
    /// which lands in the HumanConfirm range.
    pub async fn evaluate(
        &self,
        action: &str,
        context: &RiskContext,
    ) -> athen_core::error::Result<RiskScore> {
        let request = self.build_request(action);

        // Timeout the LLM risk call — risk evaluation should be fast.
        // On timeout or error, use conservative defaults that require approval.
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.router.route(&request),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::warn!("LLM risk evaluation failed: {e}, using conservative defaults");
                return Ok(self.conservative_fallback(context));
            }
            Err(_) => {
                tracing::warn!(
                    "LLM risk evaluation timed out after 10s, using conservative defaults"
                );
                return Ok(self.conservative_fallback(context));
            }
        };

        let (impact, sensitivity, confidence) = match self.parse_response(&response) {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    "LLM risk evaluation returned unparseable JSON, \
                     using trust-weighted conservative fallback"
                );
                return Ok(self.conservative_fallback(context));
            }
        };

        let effective_context = RiskContext {
            trust_level: context.trust_level,
            data_sensitivity: if (sensitivity as u32) > (context.data_sensitivity as u32) {
                sensitivity
            } else {
                context.data_sensitivity
            },
            llm_confidence: Some(confidence),
            accumulated_risk: context.accumulated_risk,
        };

        Ok(self
            .scorer
            .compute(impact, &effective_context, EvaluationMethod::LlmAssisted))
    }

    /// Return a conservative risk score when the LLM call fails or returns
    /// unparseable output. Calibrated by sender trust so an explicit "I
    /// trust this contact" signal from the user is honoured even when the
    /// risk LLM hiccups — without this an offline DeepSeek can HardBlock a
    /// trusted contact's "review this CV" email purely on fallback math.
    ///
    /// Trust-weighted defaults:
    /// - AuthUser / Trusted: read + plain + 0.5 → low score, lands in
    ///   NotifyAndProceed at worst. The user's explicit trust IS the signal.
    /// - Known: write_temp + plain + 0.4 → mid-band, HumanConfirm.
    /// - Neutral / Unknown: write_persist + personal_info + 0.3 → HardBlock,
    ///   today's behavior preserved for senders the user hasn't vetted.
    fn conservative_fallback(&self, context: &RiskContext) -> RiskScore {
        let (impact, sensitivity, confidence) = match context.trust_level {
            TrustLevel::AuthUser | TrustLevel::Trusted => {
                (BaseImpact::Read, DataSensitivity::Plain, 0.5)
            }
            TrustLevel::Known => (BaseImpact::WriteTemp, DataSensitivity::Plain, 0.4),
            TrustLevel::Neutral | TrustLevel::Unknown => {
                (BaseImpact::WritePersist, DataSensitivity::PersonalInfo, 0.3)
            }
        };
        let ctx = RiskContext {
            trust_level: context.trust_level,
            data_sensitivity: sensitivity,
            llm_confidence: Some(confidence),
            accumulated_risk: context.accumulated_risk,
        };
        self.scorer
            .compute(impact, &ctx, EvaluationMethod::LlmAssisted)
    }

    /// Build the LLM request for risk evaluation.
    pub fn build_request(&self, action: &str) -> LlmRequest {
        let system_prompt = concat!(
            "You are a risk evaluation engine for an AI agent system. ",
            "Your job is to classify the risk of a proposed action.\n\n",
            "The action is a USER REQUEST that will be executed by an AI agent with shell access, ",
            "file read/write capabilities, and internet access. Consider what the agent would ",
            "ACTUALLY DO to fulfill this request, not just the literal text.\n\n",
            "Respond ONLY with a JSON object (no markdown, no explanation) with these fields:\n",
            "- \"impact\": one of \"read\", \"write_temp\", \"write_persist\", \"system\"\n",
            "- \"sensitivity\": one of \"plain\", \"personal_info\", \"secrets\"\n",
            "- \"confidence\": a float between 0.0 and 1.0 indicating your confidence\n",
            "- \"reasoning\": a brief explanation\n\n",
            "Impact levels:\n",
            "- read: Read-only, no side effects (listing files, reading, searching)\n",
            "- write_temp: Creates temporary/reversible changes (writing to /tmp)\n",
            "- write_persist: Creates permanent/irreversible changes (writing, modifying files)\n",
            "- system: DESTRUCTIVE or DANGEROUS actions — deleting files/folders, modifying system config, ",
            "installing/removing software, killing processes, formatting disks, sending data externally, ",
            "ANY action that could cause data loss or security issues\n\n",
            "IMPORTANT: If the user asks to delete, remove, wipe, destroy, or erase files/data, ",
            "ALWAYS classify as \"system\" impact regardless of the language used. ",
            "Be conservative — when in doubt, choose a higher impact level.\n\n",
            "Sensitivity levels:\n",
            "- plain: No sensitive data involved\n",
            "- personal_info: Contains PII (names, emails, phone numbers, addresses)\n",
            "- secrets: Contains credentials, API keys, passwords, private keys\n",
        );

        let user_message = format!(
            "Evaluate the risk of this action:\n\n<<<ACTION>>>\n{}\n<<<END ACTION>>>",
            action
        );

        LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(user_message),
            }],
            max_tokens: Some(256),
            temperature: Some(0.0),
            tools: None,
            system_prompt: Some(system_prompt.to_string()),
            reasoning_effort: athen_core::llm::ReasoningEffort::default(),
        }
    }

    /// Parse the LLM response into a risk classification triple, or `None`
    /// when the response can't be decoded. The caller drives the
    /// trust-weighted conservative fallback on `None` — embedding a fixed
    /// "assume the worst" tuple here used to silently HardBlock trusted
    /// contacts on any local-model JSON wobble.
    fn parse_response(&self, response: &LlmResponse) -> Option<(BaseImpact, DataSensitivity, f64)> {
        let content = &response.content;
        let v: serde_json::Value = serde_json::from_str(content).ok()?;

        let impact = match v.get("impact")?.as_str()? {
            "read" => BaseImpact::Read,
            "write_temp" => BaseImpact::WriteTemp,
            "write_persist" => BaseImpact::WritePersist,
            "system" => BaseImpact::System,
            _ => return None,
        };

        let sensitivity = match v.get("sensitivity")?.as_str()? {
            "plain" => DataSensitivity::Plain,
            "personal_info" => DataSensitivity::PersonalInfo,
            "secrets" => DataSensitivity::Secrets,
            _ => return None,
        };

        let confidence = v
            .get("confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);

        Some((impact, sensitivity, confidence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use athen_core::contact::TrustLevel;
    use athen_core::llm::{BudgetStatus, FinishReason, LlmResponse, TokenUsage};
    use athen_core::risk::{RiskDecision, RiskLevel};

    /// Mock LLM router that returns a fixed response.
    struct MockRouter {
        response_content: String,
    }

    impl MockRouter {
        fn new(content: &str) -> Self {
            Self {
                response_content: content.to_string(),
            }
        }

        fn response(&self) -> LlmResponse {
            LlmResponse {
                content: self.response_content.clone(),
                reasoning_content: None,
                model_used: "mock-model".to_string(),
                provider: "mock".to_string(),
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    estimated_cost_usd: Some(0.001),
                },
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            }
        }
    }

    #[async_trait]
    impl LlmRouter for MockRouter {
        async fn route(&self, _request: &LlmRequest) -> athen_core::error::Result<LlmResponse> {
            Ok(self.response())
        }

        async fn budget_remaining(&self) -> athen_core::error::Result<BudgetStatus> {
            Ok(BudgetStatus {
                daily_limit_usd: Some(10.0),
                spent_today_usd: 0.0,
                remaining_usd: Some(10.0),
                tokens_used_today: 0,
            })
        }
    }

    fn default_ctx() -> RiskContext {
        RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        }
    }

    #[test]
    fn prompt_contains_action() {
        let router = MockRouter::new("{}");
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let req = evaluator.build_request("delete all user data");
        let msg = &req.messages[0];
        match &msg.content {
            MessageContent::Text(text) => {
                assert!(text.contains("delete all user data"));
                assert!(text.contains("<<<ACTION>>>"));
                assert!(text.contains("<<<END ACTION>>>"));
            }
            _ => panic!("Expected text content"),
        }
        assert!(req.system_prompt.is_some());
        assert_eq!(req.profile, ModelProfile::Fast);
        assert_eq!(req.temperature, Some(0.0));
    }

    #[tokio::test]
    async fn parses_valid_llm_response() {
        let json = r#"{"impact":"system","sensitivity":"secrets","confidence":0.95,"reasoning":"dangerous"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = default_ctx();
        let score = evaluator
            .evaluate("do something dangerous", &ctx)
            .await
            .unwrap();
        // System(90) * AuthUser(0.5) * Secrets(5) + (1-0.95)^2*100 = 225 + 0.25 = 225.25
        assert!((score.total - 225.25).abs() < 0.01);
        assert_eq!(score.evaluation_method, EvaluationMethod::LlmAssisted);
    }

    #[tokio::test]
    async fn parses_read_plain_high_confidence() {
        let json =
            r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"reasoning":"safe read"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = default_ctx();
        let score = evaluator.evaluate("read readme", &ctx).await.unwrap();
        // Read(1) * AuthUser(0.5) * Plain(1) + 0 = 0.5
        assert!((score.total - 0.5).abs() < f64::EPSILON);
        assert_eq!(score.level, RiskLevel::Safe);
    }

    #[tokio::test]
    async fn fallback_for_authuser_lands_below_human_confirm() {
        // Auth user with unparseable response should NOT escalate. The
        // trust-weighted fallback gives Read+Plain+0.5 confidence.
        let router = MockRouter::new("I don't know what to do");
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = default_ctx();
        let score = evaluator.evaluate("something", &ctx).await.unwrap();
        // Read(1) * AuthUser(0.5) * Plain(1) + (1-0.5)^2*100 = 0.5 + 25 = 25.5
        assert!((score.total - 25.5).abs() < 0.01, "got {}", score.total);
        assert!(score.total < 50.0); // never HumanConfirm or worse
    }

    #[tokio::test]
    async fn fallback_for_trusted_contact_does_not_hardblock() {
        // Regression: a Trusted contact's CV-review email used to HardBlock
        // (129) on any LLM hiccup. Trust-weighted fallback now puts them in
        // NotifyAndProceed range, honouring the user's explicit trust.
        let router = MockRouter::new("garbage not json");
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = RiskContext {
            trust_level: TrustLevel::Trusted,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate("Review CV", &ctx).await.unwrap();
        // Read(1) * Trusted(1.0) * Plain(1) + (1-0.5)^2*100 = 1 + 25 = 26
        assert!((score.total - 26.0).abs() < 0.01, "got {}", score.total);
        assert_ne!(score.decision(), RiskDecision::HardBlock);
    }

    #[tokio::test]
    async fn fallback_for_unknown_sender_still_hardblocks() {
        // The harden-on-unknown stays the same — strangers we've never
        // seen before still get the conservative WritePersist+PII+0.3
        // fallback, which Unknown's 5x multiplier pushes to HardBlock.
        let router = MockRouter::new("garbage");
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = RiskContext {
            trust_level: TrustLevel::Unknown,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate("anything", &ctx).await.unwrap();
        // WritePersist(40) * Unknown(5) * PersonalInfo(2) + 49 = 449
        assert_eq!(score.decision(), RiskDecision::HardBlock);
    }

    #[tokio::test]
    async fn falls_back_on_unknown_impact() {
        let json = r#"{"impact":"destroy","sensitivity":"plain","confidence":0.9}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        // Use a Neutral sender so the fallback is loud enough to still
        // exceed NotifyAndProceed — keeps the original test intent.
        let ctx = RiskContext {
            trust_level: TrustLevel::Neutral,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate("something", &ctx).await.unwrap();
        // WritePersist(40) * Neutral(2) * PersonalInfo(2) + 49 = 209
        assert!(score.total > 50.0, "got {}", score.total);
    }

    #[tokio::test]
    async fn context_sensitivity_not_downgraded() {
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Secrets,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate("read secret file", &ctx).await.unwrap();
        // Read(1) * AuthUser(0.5) * Secrets(5) + 0 = 2.5
        assert!((score.total - 2.5).abs() < f64::EPSILON);
    }
}
