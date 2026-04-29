//! RiskScore formula: (Ibase x Morigen x Mdatos) + Pincertidumbre

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::risk::{BaseImpact, EvaluationMethod, RiskContext, RiskLevel, RiskScore};
use athen_core::task::Task;
use athen_core::traits::coordinator::RiskEvaluator;

/// Core risk scoring engine.
///
/// Computes: `RiskScore = (Ibase * Morigen * Mdatos) + Pincertidumbre`
pub struct RiskScorer;

impl Default for RiskScorer {
    fn default() -> Self {
        Self::new()
    }
}

impl RiskScorer {
    pub fn new() -> Self {
        Self
    }

    /// Compute a risk score from raw inputs.
    pub fn compute(
        &self,
        base_impact: BaseImpact,
        context: &RiskContext,
        method: EvaluationMethod,
    ) -> RiskScore {
        let i_base = base_impact as u32 as f64;
        let m_origen = context.trust_level.risk_multiplier();
        let m_datos = context.data_sensitivity as u32 as f64;
        let confidence = context.llm_confidence.unwrap_or(1.0).clamp(0.0, 1.0);
        let p_incertidumbre = (1.0 - confidence).powi(2) * 100.0;

        let total = (i_base * m_origen * m_datos) + p_incertidumbre;
        let level = score_to_level(total);

        RiskScore {
            total,
            base_impact: i_base,
            origin_multiplier: m_origen,
            data_multiplier: m_datos,
            uncertainty_penalty: p_incertidumbre,
            level,
            evaluation_method: method,
        }
    }
}

/// Map a numeric score to a RiskLevel.
pub fn score_to_level(total: f64) -> RiskLevel {
    match total as u32 {
        0..20 => RiskLevel::Safe,
        20..50 => RiskLevel::Caution,
        50..90 => RiskLevel::Danger,
        _ => RiskLevel::Critical,
    }
}

#[async_trait]
impl RiskEvaluator for RiskScorer {
    async fn evaluate(&self, _task: &Task, context: &RiskContext) -> Result<RiskScore> {
        // Default to Read impact when evaluating a task without action-level detail.
        // In practice, the CombinedRiskEvaluator calls rules/LLM to determine BaseImpact.
        let base_impact = BaseImpact::Read;
        Ok(self.compute(base_impact, context, EvaluationMethod::RuleBased))
    }

    fn requires_approval(&self, score: &RiskScore) -> bool {
        use athen_core::risk::RiskDecision;
        matches!(
            score.decision(),
            RiskDecision::HumanConfirm | RiskDecision::HardBlock
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::contact::TrustLevel;
    use athen_core::risk::{DataSensitivity, RiskDecision};

    fn ctx(trust: TrustLevel, data: DataSensitivity, confidence: Option<f64>) -> RiskContext {
        RiskContext {
            trust_level: trust,
            data_sensitivity: data,
            llm_confidence: confidence,
            accumulated_risk: 0,
        }
    }

    // ---------- formula correctness ----------

    #[test]
    fn read_authuser_plain_full_confidence() {
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        // 1 * 0.5 * 1 + 0 = 0.5
        assert!((score.total - 0.5).abs() < f64::EPSILON);
        assert_eq!(score.level, RiskLevel::Safe);
        assert_eq!(score.decision(), RiskDecision::SilentApprove);
    }

    #[test]
    fn system_unknown_secrets_zero_confidence() {
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::System,
            &ctx(TrustLevel::Unknown, DataSensitivity::Secrets, Some(0.0)),
            EvaluationMethod::RuleBased,
        );
        // 90 * 5.0 * 5 + (1.0)^2 * 100 = 2250 + 100 = 2350
        assert!((score.total - 2350.0).abs() < f64::EPSILON);
        assert_eq!(score.level, RiskLevel::Critical);
        assert_eq!(score.decision(), RiskDecision::HardBlock);
    }

    #[test]
    fn write_persist_trusted_personal_info_half_confidence() {
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::WritePersist,
            &ctx(
                TrustLevel::Trusted,
                DataSensitivity::PersonalInfo,
                Some(0.5),
            ),
            EvaluationMethod::LlmAssisted,
        );
        // 40 * 1.0 * 2 + (0.5)^2 * 100 = 80 + 25 = 105
        assert!((score.total - 105.0).abs() < f64::EPSILON);
        assert_eq!(score.level, RiskLevel::Critical);
        assert_eq!(score.decision(), RiskDecision::HardBlock);
    }

    #[test]
    fn write_temp_neutral_plain_full_confidence() {
        let scorer = RiskScorer::new();
        let score = scorer.compute(
            BaseImpact::WriteTemp,
            &ctx(TrustLevel::Neutral, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        // 10 * 2.0 * 1 + 0 = 20
        assert!((score.total - 20.0).abs() < f64::EPSILON);
        assert_eq!(score.level, RiskLevel::Caution);
        assert_eq!(score.decision(), RiskDecision::NotifyAndProceed);
    }

    // ---------- all BaseImpact values ----------

    #[test]
    fn base_impact_values() {
        assert_eq!(BaseImpact::Read as u32, 1);
        assert_eq!(BaseImpact::WriteTemp as u32, 10);
        assert_eq!(BaseImpact::WritePersist as u32, 40);
        assert_eq!(BaseImpact::System as u32, 90);
    }

    // ---------- all TrustLevel multipliers ----------

    #[test]
    fn trust_multipliers() {
        assert!((TrustLevel::AuthUser.risk_multiplier() - 0.5).abs() < f64::EPSILON);
        assert!((TrustLevel::Trusted.risk_multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((TrustLevel::Known.risk_multiplier() - 1.5).abs() < f64::EPSILON);
        assert!((TrustLevel::Neutral.risk_multiplier() - 2.0).abs() < f64::EPSILON);
        assert!((TrustLevel::Unknown.risk_multiplier() - 5.0).abs() < f64::EPSILON);
    }

    // ---------- exhaustive combinations ----------

    #[test]
    fn all_impact_trust_data_combinations() {
        let scorer = RiskScorer::new();
        let impacts = [
            BaseImpact::Read,
            BaseImpact::WriteTemp,
            BaseImpact::WritePersist,
            BaseImpact::System,
        ];
        let trusts = [
            TrustLevel::AuthUser,
            TrustLevel::Trusted,
            TrustLevel::Known,
            TrustLevel::Neutral,
            TrustLevel::Unknown,
        ];
        let datas = [
            DataSensitivity::Plain,
            DataSensitivity::PersonalInfo,
            DataSensitivity::Secrets,
        ];

        for &impact in &impacts {
            for &trust in &trusts {
                for &data in &datas {
                    let c = ctx(trust, data, Some(1.0));
                    let score = scorer.compute(impact, &c, EvaluationMethod::RuleBased);
                    let expected =
                        (impact as u32 as f64) * trust.risk_multiplier() * (data as u32 as f64);
                    assert!(
                        (score.total - expected).abs() < f64::EPSILON,
                        "Mismatch for {:?}/{:?}/{:?}: got {}, expected {}",
                        impact,
                        trust,
                        data,
                        score.total,
                        expected,
                    );
                }
            }
        }
    }

    // ---------- uncertainty penalty ----------

    #[test]
    fn uncertainty_penalty_values() {
        let scorer = RiskScorer::new();
        // confidence 1.0 -> penalty 0
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 0.0).abs() < f64::EPSILON);

        // confidence 0.5 -> penalty 25
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.5)),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 25.0).abs() < f64::EPSILON);

        // confidence 0.1 -> penalty 81
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.1)),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 81.0).abs() < f64::EPSILON);

        // confidence 0.0 -> penalty 100
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(0.0)),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_confidence_defaults_to_full() {
        let scorer = RiskScorer::new();
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, None),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 0.0).abs() < f64::EPSILON);
    }

    // ---------- level mapping ----------

    #[test]
    fn score_to_level_boundaries() {
        assert_eq!(score_to_level(0.0), RiskLevel::Safe);
        assert_eq!(score_to_level(19.0), RiskLevel::Safe);
        assert_eq!(score_to_level(19.99), RiskLevel::Safe);
        assert_eq!(score_to_level(20.0), RiskLevel::Caution);
        assert_eq!(score_to_level(49.0), RiskLevel::Caution);
        assert_eq!(score_to_level(50.0), RiskLevel::Danger);
        assert_eq!(score_to_level(89.0), RiskLevel::Danger);
        assert_eq!(score_to_level(90.0), RiskLevel::Critical);
        assert_eq!(score_to_level(1000.0), RiskLevel::Critical);
    }

    // ---------- decision thresholds ----------

    #[test]
    fn decision_thresholds() {
        let scorer = RiskScorer::new();

        // SilentApprove: Read + AuthUser + Plain + full confidence = 0.5
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        assert_eq!(s.decision(), RiskDecision::SilentApprove);
        assert!(!scorer.requires_approval(&s));

        // NotifyAndProceed: WriteTemp + Neutral + Plain + full = 20
        let s = scorer.compute(
            BaseImpact::WriteTemp,
            &ctx(TrustLevel::Neutral, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        assert_eq!(s.decision(), RiskDecision::NotifyAndProceed);
        assert!(!scorer.requires_approval(&s));

        // HumanConfirm: WritePersist + Known + Plain + full = 60
        let s = scorer.compute(
            BaseImpact::WritePersist,
            &ctx(TrustLevel::Known, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        assert_eq!(s.decision(), RiskDecision::HumanConfirm);
        assert!(scorer.requires_approval(&s));

        // HardBlock: System + Trusted + Plain + full = 90
        let s = scorer.compute(
            BaseImpact::System,
            &ctx(TrustLevel::Trusted, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        assert_eq!(s.decision(), RiskDecision::HardBlock);
        assert!(scorer.requires_approval(&s));
    }

    // ---------- edge cases ----------

    #[test]
    fn confidence_clamped_above_one() {
        let scorer = RiskScorer::new();
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.5)),
            EvaluationMethod::RuleBased,
        );
        assert!((s.uncertainty_penalty - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn confidence_clamped_below_zero() {
        let scorer = RiskScorer::new();
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(-0.5)),
            EvaluationMethod::RuleBased,
        );
        // clamped to 0.0 -> penalty 100
        assert!((s.uncertainty_penalty - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn max_everything() {
        let scorer = RiskScorer::new();
        let s = scorer.compute(
            BaseImpact::System,
            &ctx(TrustLevel::Unknown, DataSensitivity::Secrets, Some(0.0)),
            EvaluationMethod::RuleBased,
        );
        // 90 * 5 * 5 + 100 = 2350
        assert!((s.total - 2350.0).abs() < f64::EPSILON);
        assert_eq!(s.level, RiskLevel::Critical);
        assert_eq!(s.decision(), RiskDecision::HardBlock);
    }

    #[test]
    fn min_everything() {
        let scorer = RiskScorer::new();
        let s = scorer.compute(
            BaseImpact::Read,
            &ctx(TrustLevel::AuthUser, DataSensitivity::Plain, Some(1.0)),
            EvaluationMethod::RuleBased,
        );
        // 1 * 0.5 * 1 + 0 = 0.5
        assert!((s.total - 0.5).abs() < f64::EPSILON);
        assert_eq!(s.level, RiskLevel::Safe);
        assert_eq!(s.decision(), RiskDecision::SilentApprove);
    }
}
