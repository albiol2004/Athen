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
    /// Orthogonal "this is a coding task" signal from the same LLM call.
    /// True for tasks dominated by read/edit/grep/shell over a source
    /// tree — bug fixes, small features, refactors, code review. Routes
    /// the main executor to `ModelProfile::Code` regardless of complexity
    /// when set (see `resolve_tier_from_signals`). Defaults to false /
    /// missing on regex-only scored actions and on persisted scores
    /// predating this field, so existing callers keep the old behaviour.
    #[serde(default)]
    pub is_code_task: bool,
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
///
/// Low and Medium both route to `Fast`: `Fast` is the *task-execution*
/// tier (the model that actually does the user's work — typically local
/// inference). `Cheap` is deliberately NOT a task-execution tier — it is
/// reserved for high-parallelism auxiliary calls (triage, judges,
/// classifiers, extractors), which is why the Bundle UI labels it
/// "Judges". Sending easy tasks to `Cheap` would push real task work onto
/// that auxiliary tier (often an external API), defeating the split. Only
/// `High` escalates above `Fast`, to `Powerful`.
pub fn complexity_to_tier(tag: ComplexityTag) -> ModelProfile {
    match tag {
        ComplexityTag::Low | ComplexityTag::Medium => ModelProfile::Fast,
        ComplexityTag::High => ModelProfile::Powerful,
    }
}

/// Resolve a `ModelProfile` from the orthogonal signals the risk LLM
/// emits: hardness (`ComplexityTag`) and the "this is a coding task"
/// boolean. `is_code_task` wins over complexity for Low/Medium tasks —
/// these are the cases where a Code-tuned model materially beats a
/// generalist Fast/Cheap one. High-complexity coding tasks still route
/// to `Powerful` because they're more likely to be architectural /
/// cross-module reasoning than line-level edit work — the Code tier is
/// intentionally a small/fast specialist, not a general "harder than
/// Fast" upgrade. Callers without a complexity tag pass `None` and the
/// caller-supplied default is used; if `is_code_task` is true with no
/// complexity, Code wins (we know it's code, we just don't know how
/// hard — Code is the safer default than the static label).
pub fn resolve_tier_from_signals(
    complexity: Option<ComplexityTag>,
    is_code_task: bool,
    default_label: ModelProfile,
) -> ModelProfile {
    match (complexity, is_code_task) {
        (Some(ComplexityTag::High), _) => ModelProfile::Powerful,
        (_, true) => ModelProfile::Code,
        (Some(tag), false) => complexity_to_tier(tag),
        (None, false) => default_label,
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
        // Low and Medium both route to the Fast (task-execution) tier;
        // Cheap is reserved for auxiliary "Judges" calls, never tasks.
        assert_eq!(complexity_to_tier(ComplexityTag::Low), ModelProfile::Fast);
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
        // Same rehydration must default the new boolean to false so older
        // persisted scores keep their pre-Code routing behaviour.
        assert!(!score.is_code_task);
    }

    #[test]
    fn resolve_tier_routes_code_task_to_code_tier() {
        // Low / Medium coding tasks route to Code regardless of complexity.
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::Low), true, ModelProfile::Fast),
            ModelProfile::Code
        );
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::Medium), true, ModelProfile::Fast),
            ModelProfile::Code
        );
    }

    #[test]
    fn resolve_tier_high_complexity_beats_code_flag() {
        // Architectural / cross-module reasoning still wants Powerful —
        // Code tier is a small/fast specialist, not a "harder than Fast"
        // upgrade.
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::High), true, ModelProfile::Fast),
            ModelProfile::Powerful
        );
    }

    #[test]
    fn resolve_tier_no_complexity_with_code_flag_picks_code() {
        // We know it's code, we just don't know how hard — Code beats the
        // static call-site default.
        assert_eq!(
            resolve_tier_from_signals(None, true, ModelProfile::Fast),
            ModelProfile::Code
        );
    }

    #[test]
    fn resolve_tier_non_code_paths_match_complexity_to_tier() {
        // No code flag: behaviour follows complexity_to_tier — Low and
        // Medium both land on Fast (task-execution tier).
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::Low), false, ModelProfile::Fast),
            ModelProfile::Fast
        );
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::Medium), false, ModelProfile::Fast),
            ModelProfile::Fast
        );
        assert_eq!(
            resolve_tier_from_signals(Some(ComplexityTag::High), false, ModelProfile::Fast),
            ModelProfile::Powerful
        );
        // No signal at all — fall through to the caller-supplied default.
        assert_eq!(
            resolve_tier_from_signals(None, false, ModelProfile::Powerful),
            ModelProfile::Powerful
        );
    }
}
