use serde::{Deserialize, Serialize};

use crate::contact::TrustLevel;

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
