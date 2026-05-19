//! LLM-assisted risk evaluation for ambiguous cases (step 2).
//!
//! When the fast regex rules cannot confidently classify an action,
//! we ask an LLM to evaluate its risk.

use athen_core::contact::TrustLevel;
use athen_core::llm::{ChatMessage, LlmRequest, LlmResponse, MessageContent, ModelProfile, Role};
use athen_core::risk::{
    BaseImpact, ComplexityTag, DataSensitivity, EvaluationMethod, RiskContext, RiskScore,
    TriagePlan,
};
use athen_core::traits::llm::LlmRouter;

use crate::scorer::RiskScorer;

/// Parsed payload from the LLM risk-evaluation response. Internal —
/// callers consume the assembled `RiskScore`, not this intermediate.
struct ParsedRiskResponse {
    impact: BaseImpact,
    sensitivity: DataSensitivity,
    confidence: f64,
    complexity: Option<ComplexityTag>,
    plan: Option<TriagePlan>,
}

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

        let ParsedRiskResponse {
            impact,
            sensitivity,
            confidence,
            complexity,
            plan,
        } = match self.parse_response(&response) {
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

        let mut score =
            self.scorer
                .compute(impact, &effective_context, EvaluationMethod::LlmAssisted);
        score.complexity = complexity;
        score.plan = plan;
        Ok(score)
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
            "- \"complexity\": one of \"low\", \"medium\", \"high\" — how hard the task is for the agent (NOT how risky)\n",
            "- \"acceptance_criteria\": one or two short lines describing what \"done\" looks like for this task. Concrete and testable. Empty string \"\" if the task is conversational and has no done-criterion.\n",
            "- \"scope\": one short sentence naming what the task is NOT, to fence the agent from drift. Empty string \"\" if there is no useful fence.\n",
            "- \"reasoning\": a brief explanation\n\n",
            "Complexity levels (judge difficulty, not danger — a one-line `rm -rf /` is HIGH risk but LOW complexity):\n",
            "- low: single-shot trivial — read one file, look up a fact, summarize a short message, send a one-line reply\n",
            "- medium: multi-step but standard — write a small script, edit a few files, send a structured reply, look up + summarize\n",
            "- high: open-ended / requires reasoning across many constraints — design a system, debug across modules, write substantial code, plan a multi-day workflow\n\n",
            "acceptance_criteria examples:\n",
            "- \"Reply to João's email confirming the Q3 contract terms.\" (for \"draft reply to that contract email\")\n",
            "- \"src/auth.rs no longer panics on empty tokens; test suite passes.\" (for \"fix the auth bug\")\n",
            "- \"\" (for \"hey how are you\" — no done-criterion)\n\n",
            "scope examples:\n",
            "- \"NOT a full refactor of the auth module.\" (fences a bug-fix task)\n",
            "- \"NOT a multi-message conversation — single reply only.\" (fences an email task)\n",
            "- \"\" (when no fence helps)\n\n",
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
            // Bumped from 256 to fit acceptance_criteria + scope alongside
            // the existing impact/sensitivity/complexity/reasoning fields.
            max_tokens: Some(512),
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
    fn parse_response(&self, response: &LlmResponse) -> Option<ParsedRiskResponse> {
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

        // Complexity is best-effort: a missing or off-vocabulary tag falls
        // through to the static call-site tier rather than aborting the
        // whole risk parse over a routing hint.
        let complexity = v.get("complexity").and_then(|c| c.as_str()).and_then(|s| {
            match s.trim().to_ascii_lowercase().as_str() {
                "low" => Some(ComplexityTag::Low),
                "medium" | "med" => Some(ComplexityTag::Medium),
                "high" => Some(ComplexityTag::High),
                _ => None,
            }
        });

        // Plan is best-effort like complexity. Both fields must be present
        // and non-empty after trimming for the plan to be useful; otherwise
        // we drop it cleanly rather than persisting a half-formed plan that
        // would mislead the compactor + judge downstream.
        let plan = {
            let acceptance = v
                .get("acceptance_criteria")
                .and_then(|c| c.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let scope = v
                .get("scope")
                .and_then(|c| c.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (acceptance, scope) {
                (Some(a), Some(s)) => Some(TriagePlan {
                    acceptance_criteria: a.to_string(),
                    scope: s.to_string(),
                }),
                _ => None,
            }
        };

        Some(ParsedRiskResponse {
            impact,
            sensitivity,
            confidence,
            complexity,
            plan,
        })
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
    async fn parses_complexity_when_present() {
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"complexity":"high","reasoning":"hard"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("design a system", &default_ctx())
            .await
            .unwrap();
        assert_eq!(
            score.complexity,
            Some(athen_core::risk::ComplexityTag::High)
        );
    }

    #[tokio::test]
    async fn complexity_missing_falls_through_to_none() {
        // Older / smaller models that ignore the new rubric must not break
        // the score; complexity stays None and the executor falls back to
        // the static call-site tier.
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("read a file", &default_ctx())
            .await
            .unwrap();
        assert!(score.complexity.is_none());
    }

    #[tokio::test]
    async fn complexity_off_vocabulary_is_ignored() {
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"complexity":"galaxy-brain"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("anything", &default_ctx())
            .await
            .unwrap();
        assert!(score.complexity.is_none());
    }

    #[test]
    fn regex_scorer_emits_no_complexity() {
        // Regex can't classify hardness; the path must always emit None so
        // the executor falls through to the static tier rather than
        // routing on a guess.
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::Read,
            &default_ctx(),
            EvaluationMethod::RuleBased,
        );
        assert!(score.complexity.is_none());
    }

    #[tokio::test]
    async fn parses_plan_when_both_fields_present() {
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"acceptance_criteria":"Reply to João confirming Q3 terms.","scope":"NOT a multi-message thread."}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("draft reply", &default_ctx())
            .await
            .unwrap();
        let plan = score.plan.expect("plan should be parsed");
        assert_eq!(
            plan.acceptance_criteria,
            "Reply to João confirming Q3 terms."
        );
        assert_eq!(plan.scope, "NOT a multi-message thread.");
    }

    #[tokio::test]
    async fn plan_missing_falls_through_to_none() {
        // Older / smaller models that ignore the new rubric must not break
        // the score; plan stays None and downstream consumers (executor,
        // compactor, judge) fall through to plan-less behaviour.
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("anything", &default_ctx())
            .await
            .unwrap();
        assert!(score.plan.is_none());
    }

    #[tokio::test]
    async fn plan_with_only_acceptance_is_dropped() {
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"acceptance_criteria":"foo done"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("anything", &default_ctx())
            .await
            .unwrap();
        assert!(
            score.plan.is_none(),
            "half-formed plan must not persist — both fields required"
        );
    }

    #[tokio::test]
    async fn plan_with_only_scope_is_dropped() {
        let json =
            r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"scope":"NOT a refactor"}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("anything", &default_ctx())
            .await
            .unwrap();
        assert!(score.plan.is_none());
    }

    #[tokio::test]
    async fn plan_with_empty_strings_is_dropped() {
        // Models tend to emit empty-string placeholders for fields they
        // don't know how to fill. Treat as missing — empty acceptance
        // would poison the completion judge's mismatch check.
        let json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"acceptance_criteria":"   ","scope":""}"#;
        let router = MockRouter::new(json);
        let evaluator = LlmRiskEvaluator::new(Box::new(router));
        let score = evaluator
            .evaluate("hey how are you", &default_ctx())
            .await
            .unwrap();
        assert!(score.plan.is_none());
    }

    #[test]
    fn regex_scorer_emits_no_plan() {
        // Regex can't draft plans either — same fall-through as complexity.
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::Read,
            &default_ctx(),
            EvaluationMethod::RuleBased,
        );
        assert!(score.plan.is_none());
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
