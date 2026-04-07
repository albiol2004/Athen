//! Integration tests: risk system + contacts/trust system.
//!
//! These tests verify that the risk scoring engine interacts correctly
//! with the trust-level system from `athen-contacts`, producing the
//! expected risk decisions as trust evolves over time.

use async_trait::async_trait;

use athen_contacts::trust::TrustManager;
use athen_contacts::InMemoryContactStore;
use athen_core::contact::{IdentifierKind, TrustLevel};
use athen_core::llm::{
    BudgetStatus, FinishReason, LlmRequest, LlmResponse, TokenUsage,
};
use athen_core::risk::{
    BaseImpact, DataSensitivity, EvaluationMethod, RiskContext, RiskDecision,
};
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::traits::llm::LlmRouter;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::rules::RuleEngine;
use athen_risk::scorer::RiskScorer;
use athen_risk::CombinedRiskEvaluator;

use athen_core::traits::coordinator::RiskEvaluator;
use chrono::Utc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ctx(
    trust: TrustLevel,
    data: DataSensitivity,
    confidence: Option<f64>,
) -> RiskContext {
    RiskContext {
        trust_level: trust,
        data_sensitivity: data,
        llm_confidence: confidence,
        accumulated_risk: 0,
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

/// A mock LLM router that returns a fixed JSON response.
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
    async fn route(&self, _request: &LlmRequest) -> athen_core::error::Result<LlmResponse> {
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

    async fn budget_remaining(&self) -> athen_core::error::Result<BudgetStatus> {
        Ok(BudgetStatus {
            daily_limit_usd: None,
            spent_today_usd: 0.0,
            remaining_usd: None,
            tokens_used_today: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Test 1: Same action, different trust levels
// ---------------------------------------------------------------------------

#[test]
fn test_same_action_different_trust_levels() {
    let scorer = RiskScorer::new();

    // "send email" action => WritePersist = 40, Plain data, full confidence.

    // AuthUser (0.5x): 40 * 0.5 * 1 + 0 = 20 => NotifyAndProceed
    let score_auth = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_auth.total - 20.0).abs() < f64::EPSILON,
        "AuthUser score should be 20, got {}",
        score_auth.total
    );
    assert_eq!(score_auth.decision(), RiskDecision::NotifyAndProceed);

    // Known (1.5x): 40 * 1.5 * 1 + 0 = 60 => HumanConfirm
    let score_known = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::Known, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_known.total - 60.0).abs() < f64::EPSILON,
        "Known score should be 60, got {}",
        score_known.total
    );
    assert_eq!(score_known.decision(), RiskDecision::HumanConfirm);

    // Unknown (5.0x): 40 * 5.0 * 1 + 0 = 200 => HardBlock
    let score_unknown = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::Unknown, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_unknown.total - 200.0).abs() < f64::EPSILON,
        "Unknown score should be 200, got {}",
        score_unknown.total
    );
    assert_eq!(score_unknown.decision(), RiskDecision::HardBlock);

    // Verify the progression: decisions escalate with decreasing trust.
    assert!(score_auth.total < score_known.total);
    assert!(score_known.total < score_unknown.total);
}

// ---------------------------------------------------------------------------
// Test 2: Trust evolution changes risk decisions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_trust_evolution_changes_risk_decisions() {
    let store = InMemoryContactStore::new();
    let manager = TrustManager::new(Box::new(store));
    let scorer = RiskScorer::new();

    // Resolve a new contact — starts at T0 Unknown (5.0x).
    let contact = manager
        .resolve_contact("stranger@example.com", IdentifierKind::Email)
        .await
        .unwrap();
    let id = contact.id;
    assert_eq!(contact.trust_level, TrustLevel::Unknown);

    // Evaluate "draft email" (WriteTemp=10) from this unknown contact.
    // 10 * 5.0 * 1 + 0 = 50 => HumanConfirm
    let score_t0 = scorer.compute(
        BaseImpact::WriteTemp,
        &ctx(contact.trust_level, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_t0.total - 50.0).abs() < f64::EPSILON,
        "T0 score should be 50, got {}",
        score_t0.total
    );
    assert_eq!(score_t0.decision(), RiskDecision::HumanConfirm);

    // Record 5 approvals => upgrades T0 -> T1 Neutral (2.0x).
    for _ in 0..5 {
        manager.record_approval(id).await.unwrap();
    }
    let contact_t1 = manager
        .find_by_identifier("stranger@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(contact_t1.trust_level, TrustLevel::Neutral);

    // Same action with T1: 10 * 2.0 * 1 + 0 = 20 => NotifyAndProceed
    let score_t1 = scorer.compute(
        BaseImpact::WriteTemp,
        &ctx(contact_t1.trust_level, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_t1.total - 20.0).abs() < f64::EPSILON,
        "T1 score should be 20, got {}",
        score_t1.total
    );
    assert_eq!(score_t1.decision(), RiskDecision::NotifyAndProceed);

    // Record 5 more approvals => upgrades T1 -> T2 Known (1.5x).
    for _ in 0..5 {
        manager.record_approval(id).await.unwrap();
    }
    let contact_t2 = manager
        .find_by_identifier("stranger@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(contact_t2.trust_level, TrustLevel::Known);

    // Same action with T2: 10 * 1.5 * 1 + 0 = 15 => SilentApprove
    let score_t2 = scorer.compute(
        BaseImpact::WriteTemp,
        &ctx(contact_t2.trust_level, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_t2.total - 15.0).abs() < f64::EPSILON,
        "T2 score should be 15, got {}",
        score_t2.total
    );
    assert_eq!(score_t2.decision(), RiskDecision::SilentApprove);

    // Assert the progression: scores decrease as trust increases.
    assert!(score_t0.total > score_t1.total);
    assert!(score_t1.total > score_t2.total);
}

// ---------------------------------------------------------------------------
// Test 3: Rule engine with real risk context
// ---------------------------------------------------------------------------

#[test]
fn test_rule_engine_with_real_risk_context() {
    let engine = RuleEngine::new();

    // 1. "rm -rf /tmp/test" with AuthUser context.
    //    Detects dangerous shell => System(90) * AuthUser(0.5) * Plain(1) + 0 = 45
    let ctx_auth = ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0));
    let score_auth = engine.evaluate("rm -rf /tmp/test", &ctx_auth).unwrap();
    assert!(
        (score_auth.total - 45.0).abs() < f64::EPSILON,
        "AuthUser rm -rf should be 45, got {}",
        score_auth.total
    );
    assert_eq!(score_auth.decision(), RiskDecision::NotifyAndProceed);

    // 2. Same command from Unknown sender.
    //    System(90) * Unknown(5.0) * Plain(1) + 0 = 450 => HardBlock
    let ctx_unknown = ctx(TrustLevel::Unknown, DataSensitivity::Plain, Some(1.0));
    let score_unknown = engine.evaluate("rm -rf /tmp/test", &ctx_unknown).unwrap();
    assert!(
        (score_unknown.total - 450.0).abs() < f64::EPSILON,
        "Unknown rm -rf should be 450, got {}",
        score_unknown.total
    );
    assert_eq!(score_unknown.decision(), RiskDecision::HardBlock);
    assert!(score_unknown.total > score_auth.total * 5.0);

    // 3. "read the latest emails" — ambiguous, no rules match => returns None (needs LLM).
    let result_ambiguous = engine.evaluate("read the latest emails", &ctx_auth);
    assert!(
        result_ambiguous.is_none(),
        "Ambiguous action should return None for LLM fallback"
    );

    // 4. "Transfer $500 to account" from Unknown.
    //    Financial keyword => WritePersist(40), PersonalInfo(2).
    //    40 * Unknown(5.0) * PersonalInfo(2) + 0 = 400 => HardBlock
    let score_financial = engine
        .evaluate("Transfer $500 to account", &ctx_unknown)
        .unwrap();
    assert!(
        (score_financial.total - 400.0).abs() < f64::EPSILON,
        "Financial from Unknown should be 400, got {}",
        score_financial.total
    );
    assert_eq!(score_financial.decision(), RiskDecision::HardBlock);
}

// ---------------------------------------------------------------------------
// Test 4: Data sensitivity escalation
// ---------------------------------------------------------------------------

#[test]
fn test_data_sensitivity_escalation() {
    let scorer = RiskScorer::new();

    // Same action (WritePersist=40) with Trusted (1.0x), full confidence.

    // Plain (1x): 40 * 1.0 * 1 = 40
    let score_plain = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::Trusted, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_plain.total - 40.0).abs() < f64::EPSILON,
        "Plain score should be 40, got {}",
        score_plain.total
    );

    // PersonalInfo (2x): 40 * 1.0 * 2 = 80
    let score_personal = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::Trusted, DataSensitivity::PersonalInfo, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_personal.total - 80.0).abs() < f64::EPSILON,
        "PersonalInfo score should be 80, got {}",
        score_personal.total
    );

    // Secrets (5x): 40 * 1.0 * 5 = 200
    let score_secrets = scorer.compute(
        BaseImpact::WritePersist,
        &ctx(TrustLevel::Trusted, DataSensitivity::Secrets, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (score_secrets.total - 200.0).abs() < f64::EPSILON,
        "Secrets score should be 200, got {}",
        score_secrets.total
    );

    // Assert proportional increases.
    assert!(
        (score_personal.total - score_plain.total * 2.0).abs() < f64::EPSILON,
        "PersonalInfo should be exactly 2x Plain"
    );
    assert!(
        (score_secrets.total - score_plain.total * 5.0).abs() < f64::EPSILON,
        "Secrets should be exactly 5x Plain"
    );

    // Assert decisions escalate.
    assert_eq!(score_plain.decision(), RiskDecision::NotifyAndProceed);
    assert_eq!(score_personal.decision(), RiskDecision::HumanConfirm);
    assert_eq!(score_secrets.decision(), RiskDecision::HardBlock);
}

// ---------------------------------------------------------------------------
// Test 5: Uncertainty penalty impact
// ---------------------------------------------------------------------------

#[test]
fn test_uncertainty_penalty_impact() {
    let scorer = RiskScorer::new();

    // Use a low-risk base action: Read(1) * AuthUser(0.5) * Plain(1) = 0.5
    // so the uncertainty penalty is the dominant factor.

    // Confidence 1.0 => penalty = 0.0, total = 0.5
    let s_100 = scorer.compute(
        BaseImpact::Read,
        &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (s_100.uncertainty_penalty - 0.0).abs() < f64::EPSILON,
        "Penalty at 1.0 confidence should be 0, got {}",
        s_100.uncertainty_penalty
    );

    // Confidence 0.9 => penalty = (0.1)^2 * 100 = 1.0, total = 1.5
    let s_090 = scorer.compute(
        BaseImpact::Read,
        &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.9)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (s_090.uncertainty_penalty - 1.0).abs() < 1e-10,
        "Penalty at 0.9 confidence should be 1, got {}",
        s_090.uncertainty_penalty
    );

    // Confidence 0.5 => penalty = (0.5)^2 * 100 = 25.0, total = 25.5
    let s_050 = scorer.compute(
        BaseImpact::Read,
        &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.5)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (s_050.uncertainty_penalty - 25.0).abs() < f64::EPSILON,
        "Penalty at 0.5 confidence should be 25, got {}",
        s_050.uncertainty_penalty
    );

    // Confidence 0.1 => penalty = (0.9)^2 * 100 = 81.0, total = 81.5
    let s_010 = scorer.compute(
        BaseImpact::Read,
        &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.1)),
        EvaluationMethod::RuleBased,
    );
    assert!(
        (s_010.uncertainty_penalty - 81.0).abs() < f64::EPSILON,
        "Penalty at 0.1 confidence should be 81, got {}",
        s_010.uncertainty_penalty
    );

    // Low confidence can push a safe action into requiring approval.
    // At 1.0 confidence: total 0.5 => SilentApprove
    assert_eq!(s_100.decision(), RiskDecision::SilentApprove);

    // At 0.5 confidence: total 25.5 => NotifyAndProceed
    assert_eq!(s_050.decision(), RiskDecision::NotifyAndProceed);

    // At 0.1 confidence: total 81.5 => HumanConfirm
    assert_eq!(s_010.decision(), RiskDecision::HumanConfirm);
}

// ---------------------------------------------------------------------------
// Test 6: Combined evaluator chooses rules or LLM
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_combined_evaluator_chooses_rules_or_llm() {
    // Mock LLM that returns a benign read/plain classification.
    let llm_json = r#"{"impact":"read","sensitivity":"plain","confidence":1.0,"reasoning":"safe"}"#;
    let router = MockRouter::new(llm_json);
    let evaluator = CombinedRiskEvaluator::new(LlmRiskEvaluator::new(Box::new(router)));

    // 1. "sudo apt install vim" — rules detect "sudo" => returns Some => RuleBased.
    let task_sudo = make_task("sudo apt install vim");
    let ctx_auth = ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0));
    let score_sudo = evaluator.evaluate(&task_sudo, &ctx_auth).await.unwrap();
    assert_eq!(
        score_sudo.evaluation_method,
        EvaluationMethod::RuleBased,
        "sudo command should be handled by rules"
    );

    // 2. "help me organize my files" — no rules match => falls through to LLM.
    //    Use a non-AuthUser trust level so the evaluator does not short-circuit
    //    to a safe score (AuthUser + no rule match = safe, by design).
    let ctx_known = ctx(TrustLevel::Known, DataSensitivity::Plain, Some(1.0));
    let task_ambiguous = make_task("help me organize my files");
    let score_ambiguous = evaluator
        .evaluate(&task_ambiguous, &ctx_known)
        .await
        .unwrap();
    assert_eq!(
        score_ambiguous.evaluation_method,
        EvaluationMethod::LlmAssisted,
        "Ambiguous action from non-AuthUser should fall through to LLM"
    );

    // Verify that the rule-based result carries expected risk values.
    // sudo => System(90) * AuthUser(0.5) * Plain(1) = 45
    assert!(
        (score_sudo.total - 45.0).abs() < f64::EPSILON,
        "sudo score should be 45, got {}",
        score_sudo.total
    );

    // The LLM mock returns read/plain/1.0 => Read(1) * Known(1.5) * Plain(1) = 1.5
    assert!(
        (score_ambiguous.total - 1.5).abs() < f64::EPSILON,
        "LLM-evaluated score should be 1.5, got {}",
        score_ambiguous.total
    );
}
