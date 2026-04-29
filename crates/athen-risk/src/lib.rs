//! Risk evaluation engine for Athen.
//!
//! Two-step evaluation: fast regex rules, then LLM fallback for ambiguous cases.

pub mod llm_fallback;
pub mod path_eval;
pub mod rules;
pub mod scorer;

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::risk::{RiskContext, RiskDecision, RiskScore};
use athen_core::task::Task;
use athen_core::traits::coordinator::RiskEvaluator;

use athen_core::contact::TrustLevel;
use athen_core::risk::{BaseImpact, DataSensitivity, EvaluationMethod};

use crate::llm_fallback::LlmRiskEvaluator;
use crate::rules::RuleEngine;
use crate::scorer::RiskScorer;

/// Combined risk evaluator: tries fast regex rules first,
/// falls back to LLM for ambiguous cases.
pub struct CombinedRiskEvaluator {
    rules: RuleEngine,
    llm: LlmRiskEvaluator,
}

impl CombinedRiskEvaluator {
    pub fn new(llm: LlmRiskEvaluator) -> Self {
        Self {
            rules: RuleEngine::new(),
            llm,
        }
    }
}

#[async_trait]
impl RiskEvaluator for CombinedRiskEvaluator {
    async fn evaluate(&self, task: &Task, context: &RiskContext) -> Result<RiskScore> {
        // Step 1: Try fast regex rules on the task description.
        if let Some(score) = self.rules.evaluate(&task.description, context) {
            tracing::debug!(
                task_id = %task.id,
                score = score.total,
                level = ?score.level,
                "Risk evaluated via rules"
            );
            return Ok(score);
        }

        // Step 2: If the user is the authenticated owner and rules found nothing
        // dangerous, skip the LLM fallback entirely. The LLM risk check was
        // designed for ambiguous external messages — direct user input from an
        // authenticated user should never be flagged just because a small local
        // model returns invalid JSON.
        if context.trust_level == TrustLevel::AuthUser {
            tracing::debug!(
                task_id = %task.id,
                "Rules inconclusive but sender is AuthUser, returning safe score"
            );
            let scorer = RiskScorer::new();
            let safe_ctx = RiskContext {
                trust_level: TrustLevel::AuthUser,
                data_sensitivity: DataSensitivity::Plain,
                llm_confidence: Some(1.0),
                accumulated_risk: context.accumulated_risk,
            };
            return Ok(scorer.compute(BaseImpact::Read, &safe_ctx, EvaluationMethod::RuleBased));
        }

        // Step 3: Fall back to LLM for ambiguous cases from external sources.
        tracing::debug!(
            task_id = %task.id,
            "Rules inconclusive, falling back to LLM risk evaluation"
        );
        let score = self.llm.evaluate(&task.description, context).await?;
        tracing::debug!(
            task_id = %task.id,
            score = score.total,
            level = ?score.level,
            "Risk evaluated via LLM"
        );
        Ok(score)
    }

    fn requires_approval(&self, score: &RiskScore) -> bool {
        matches!(
            score.decision(),
            RiskDecision::HumanConfirm | RiskDecision::HardBlock
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use athen_core::llm::{BudgetStatus, FinishReason, LlmRequest, LlmResponse, TokenUsage};
    use athen_core::risk::RiskLevel;
    use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
    use athen_core::traits::llm::LlmRouter;
    use chrono::Utc;
    use uuid::Uuid;

    struct MockRouter {
        response_content: String,
    }

    impl MockRouter {
        fn new(content: &str) -> Self {
            Self {
                response_content: content.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmRouter for MockRouter {
        async fn route(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: self.response_content.clone(),
                reasoning_content: None,
                model_used: "mock".to_string(),
                provider: "mock".to_string(),
                usage: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 10,
                    total_tokens: 20,
                    estimated_cost_usd: None,
                },
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            })
        }

        async fn budget_remaining(&self) -> Result<BudgetStatus> {
            Ok(BudgetStatus {
                daily_limit_usd: None,
                spent_today_usd: 0.0,
                remaining_usd: None,
                tokens_used_today: 0,
            })
        }
    }

    fn make_task(description: &str) -> Task {
        let now = Utc::now();
        Task {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            source_event: None,
            domain: DomainType::Base,
            description: description.to_string(),
            priority: TaskPriority::Normal,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: vec![],
            deadline: None,
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

    #[tokio::test]
    async fn uses_rules_for_dangerous_patterns() {
        let llm_json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(llm_json);
        let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

        let task = make_task("sudo rm -rf /tmp/data");
        let ctx = default_ctx();
        let score = evaluator.evaluate(&task, &ctx).await.unwrap();

        // Should be rule-based, not LLM
        assert_eq!(score.evaluation_method, EvaluationMethod::RuleBased);
        assert_eq!(score.level, RiskLevel::Caution);
    }

    #[tokio::test]
    async fn falls_back_to_llm_for_ambiguous() {
        let llm_json =
            r#"{"impact":"write_persist","sensitivity":"personal_info","confidence":0.8}"#;
        let router = MockRouter::new(llm_json);
        let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

        let task = make_task("organize my photos into folders");
        // Use a non-AuthUser trust level so the evaluator falls through to LLM
        let ctx = RiskContext {
            trust_level: TrustLevel::Known,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate(&task, &ctx).await.unwrap();

        assert_eq!(score.evaluation_method, EvaluationMethod::LlmAssisted);
    }

    #[tokio::test]
    async fn requires_approval_for_high_risk() {
        let llm_json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(llm_json);
        let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

        // System-level action detected by rules
        let task = make_task("sudo systemctl stop nginx");
        let ctx = RiskContext {
            trust_level: TrustLevel::Unknown,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate(&task, &ctx).await.unwrap();

        // System(90) * Unknown(5.0) * Plain(1) + 0 = 450 -> HardBlock
        assert!(evaluator.requires_approval(&score));
    }

    #[tokio::test]
    async fn auth_user_benign_input_skips_llm() {
        // The mock LLM returns invalid JSON — if the evaluator called it,
        // the conservative fallback would produce a high score (HumanConfirm).
        // But since the sender is AuthUser, the evaluator should skip the LLM
        // and return a safe score directly.
        let router = MockRouter::new("this is not valid json at all");
        let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

        let task = make_task("Quien es mi novia?");
        let ctx = RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        };
        let score = evaluator.evaluate(&task, &ctx).await.unwrap();

        assert_eq!(score.level, RiskLevel::Safe);
        assert_eq!(score.evaluation_method, EvaluationMethod::RuleBased);
        assert!(!evaluator.requires_approval(&score));
        // Read(1) * AuthUser(0.5) * Plain(1) + 0 = 0.5
        assert!(
            score.total < 1.0,
            "Score should be ~0.5, got {}",
            score.total
        );
    }

    #[tokio::test]
    async fn does_not_require_approval_for_low_risk() {
        let llm_json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0}"#;
        let router = MockRouter::new(llm_json);
        let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

        // Ambiguous but LLM says read/plain/high confidence
        let task = make_task("list files in directory");
        let ctx = default_ctx();
        let score = evaluator.evaluate(&task, &ctx).await.unwrap();

        assert!(!evaluator.requires_approval(&score));
    }
}
