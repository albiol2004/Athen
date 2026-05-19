use serde::{Deserialize, Serialize};

use crate::contact::TrustLevel;
use crate::llm::ModelProfile;

/// The computed risk score for an action or task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskScore {
    pub total: f64,
    pub base_impact: f64,
    pub origin_multiplier: f64,
    pub data_multiplier: f64,
    pub uncertainty_penalty: f64,
    pub level: RiskLevel,
    pub evaluation_method: EvaluationMethod,
    /// LLM-judged task hardness, piggybacked on the risk-evaluation step
    /// so we don't pay for a second classification round-trip. Drives the
    /// main-executor tier selection via `complexity_to_tier`. `None` for
    /// regex-only scored actions (regex can't classify hardness) and for
    /// any persisted score predating this field — both fall through to
    /// the static call-site tier.
    #[serde(default)]
    pub complexity: Option<ComplexityTag>,
    /// LLM-drafted mini-plan piggybacked on the risk evaluation step. The
    /// compactor uses both fields (acceptance_criteria + scope) to decide
    /// what to keep verbatim and what to drop. The completion judge uses
    /// `acceptance_criteria` only — scope on the judge would invite false
    /// "out-of-scope" failures when the user actually wanted the side-quest.
    /// `None` for regex-only scored actions and for any persisted score
    /// predating this field.
    #[serde(default)]
    pub plan: Option<TriagePlan>,
}

/// A tiny task plan drafted by the same LLM call that evaluates risk, so
/// we don't pay for a second classification round-trip. Both fields are
/// short by design — this rides in the static prefix and survives
/// compaction as arc-level metadata, not as a chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriagePlan {
    /// One to three lines describing what "done" looks like for this task.
    /// Fed to the completion judge and used by the compactor as a relevance
    /// signal (turns mentioning these tokens are sticky).
    pub acceptance_criteria: String,
    /// One sentence describing what the task is NOT — drift fence used by
    /// the compactor to drop turns that wandered off-mission. Never fed to
    /// the completion judge.
    pub scope: String,
}

/// LLM-judged hardness of a task. Mapped to a `ModelProfile` via
/// `complexity_to_tier`. Kept tiny on purpose — three tiers is enough
/// signal to route Cheap/Fast/Powerful without overfitting the rubric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComplexityTag {
    Low,
    Medium,
    High,
}

/// Map a complexity tag to the model tier the main executor should use.
/// Hardcoded today; a per-profile mapping is a future option but YAGNI
/// while there's exactly one main-executor call site honouring this.
pub fn complexity_to_tier(tag: ComplexityTag) -> ModelProfile {
    match tag {
        ComplexityTag::Low => ModelProfile::Cheap,
        ComplexityTag::Medium => ModelProfile::Fast,
        ComplexityTag::High => ModelProfile::Powerful,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// L1: Read-only, safe
    Safe = 1,
    /// L2: Local reversible writes
    Caution = 2,
    /// L3: External / irreversible
    Danger = 3,
    /// L4: Financial / critical config
    Critical = 4,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EvaluationMethod {
    RuleBased,
    LlmAssisted,
}

/// Base impact classification for an action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BaseImpact {
    Read = 1,
    WriteTemp = 10,
    WritePersist = 40,
    System = 90,
}

/// Data sensitivity classification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DataSensitivity {
    Plain = 1,
    PersonalInfo = 2,
    Secrets = 5,
}

/// Context for risk evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskContext {
    pub trust_level: TrustLevel,
    pub data_sensitivity: DataSensitivity,
    pub llm_confidence: Option<f64>,
    pub accumulated_risk: u32,
}

/// What the system should do based on risk score.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskDecision {
    /// 0-19: Execute silently, debug log
    SilentApprove,
    /// 20-49: Execute, send notification
    NotifyAndProceed,
    /// 50-89: Pause, wait for human approval
    HumanConfirm,
    /// 90+: Block automatically
    HardBlock,
}

impl RiskScore {
    /// Determine the action based on the total risk score.
    pub fn decision(&self) -> RiskDecision {
        match self.total as u32 {
            0..20 => RiskDecision::SilentApprove,
            20..50 => RiskDecision::NotifyAndProceed,
            50..90 => RiskDecision::HumanConfirm,
            _ => RiskDecision::HardBlock,
        }
    }
}

#[cfg(test)]
mod complexity_tests {
    use super::*;

    #[test]
    fn complexity_to_tier_maps_expected() {
        assert_eq!(complexity_to_tier(ComplexityTag::Low), ModelProfile::Cheap);
        assert_eq!(
            complexity_to_tier(ComplexityTag::Medium),
            ModelProfile::Fast
        );
        assert_eq!(
            complexity_to_tier(ComplexityTag::High),
            ModelProfile::Powerful
        );
    }

    #[test]
    fn complexity_tag_serializes_to_lowercase() {
        assert_eq!(
            serde_json::to_string(&ComplexityTag::Low).unwrap(),
            "\"low\""
        );
        assert_eq!(
            serde_json::to_string(&ComplexityTag::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&ComplexityTag::High).unwrap(),
            "\"high\""
        );
        let back: ComplexityTag = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(back, ComplexityTag::High);
    }

    #[test]
    fn risk_score_complexity_defaults_to_none_when_missing() {
        // Existing persisted scores without the new field should rehydrate
        // with `complexity: None`, falling through to the static tier.
        let raw = serde_json::json!({
            "total": 12.5,
            "base_impact": 1.0,
            "origin_multiplier": 0.5,
            "data_multiplier": 1.0,
            "uncertainty_penalty": 0.0,
            "level": "Safe",
            "evaluation_method": "RuleBased"
        });
        let score: RiskScore = serde_json::from_value(raw).unwrap();
        assert!(score.complexity.is_none());
    }
}
