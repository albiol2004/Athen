//! Deterministic per-path risk evaluator.
//!
//! No LLM calls — applies a fixed hierarchy of rules and falls back to the
//! shared `RiskScorer` for the final numeric score so the result lands in the
//! correct decision band.

use std::path::Path;

use async_trait::async_trait;
use uuid::Uuid;

use athen_core::contact::TrustLevel;
use athen_core::error::Result;
use athen_core::paths;
use athen_core::risk::{
    BaseImpact, DataSensitivity, EvaluationMethod, RiskContext, RiskScore,
};

use crate::scorer::RiskScorer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
}

#[async_trait]
pub trait GrantLookup: Send + Sync {
    async fn check(&self, arc_id: Uuid, path: &Path, write: bool) -> Result<bool>;
}

pub struct PathRiskEvaluator<G: GrantLookup> {
    grants: G,
    scorer: RiskScorer,
}

impl<G: GrantLookup> PathRiskEvaluator<G> {
    pub fn new(grants: G) -> Self {
        Self {
            grants,
            scorer: RiskScorer::new(),
        }
    }

    /// Evaluate a path-touching operation. No LLM call.
    pub async fn evaluate(
        &self,
        arc_id: Uuid,
        path: &Path,
        access: PathAccess,
        context: &RiskContext,
    ) -> Result<RiskScore> {
        let write = access == PathAccess::Write;

        // 1. System path + write -> HardBlock, never overridable.
        if write && paths::is_system_path(path) {
            // Force the score above the HardBlock threshold (>= 90).
            let ctx = RiskContext {
                trust_level: TrustLevel::Unknown,
                data_sensitivity: DataSensitivity::Secrets,
                llm_confidence: Some(0.0),
                accumulated_risk: context.accumulated_risk,
            };
            return Ok(self.scorer.compute(
                BaseImpact::System,
                &ctx,
                EvaluationMethod::RuleBased,
            ));
        }

        // 2. Granted -> Safe.
        if self.grants.check(arc_id, path, write).await? {
            let impact = if write {
                BaseImpact::WriteTemp
            } else {
                BaseImpact::Read
            };
            let ctx = safe_ctx(context);
            return Ok(self.scorer.compute(impact, &ctx, EvaluationMethod::RuleBased));
        }

        // 3. Inside athen_data_dir -> Safe.
        if let Some(data) = paths::athen_data_dir() {
            if paths::path_within(path, &data) {
                let impact = if write {
                    BaseImpact::WriteTemp
                } else {
                    BaseImpact::Read
                };
                let ctx = safe_ctx(context);
                return Ok(self.scorer.compute(impact, &ctx, EvaluationMethod::RuleBased));
            }
        }

        // 4 & 5. Inside $HOME.
        if let Some(home) = paths::home_dir() {
            if paths::path_within(path, &home) {
                if write {
                    // 5. HumanConfirm — push the score into 50..90.
                    let ctx = home_write_ctx(context);
                    return Ok(self.scorer.compute(
                        BaseImpact::WritePersist,
                        &ctx,
                        EvaluationMethod::RuleBased,
                    ));
                }
                // 4. Caution — score in 20..50.
                let ctx = caution_ctx(context);
                return Ok(self.scorer.compute(
                    BaseImpact::Read,
                    &ctx,
                    EvaluationMethod::RuleBased,
                ));
            }
        }

        // 6 & 7. Outside $HOME, outside system.
        if write {
            let ctx = home_write_ctx(context);
            return Ok(self.scorer.compute(
                BaseImpact::WritePersist,
                &ctx,
                EvaluationMethod::RuleBased,
            ));
        }
        let ctx = caution_ctx(context);
        Ok(self.scorer.compute(BaseImpact::Read, &ctx, EvaluationMethod::RuleBased))
    }
}

fn safe_ctx(base: &RiskContext) -> RiskContext {
    RiskContext {
        trust_level: TrustLevel::AuthUser,
        data_sensitivity: DataSensitivity::Plain,
        llm_confidence: Some(1.0),
        accumulated_risk: base.accumulated_risk,
    }
}

/// Push a Read into the Caution band (20..50).
/// Read(1) * Neutral(2) * Plain(1) = 2; needs uncertainty penalty ~25 -> conf 0.5.
fn caution_ctx(base: &RiskContext) -> RiskContext {
    RiskContext {
        trust_level: TrustLevel::Neutral,
        data_sensitivity: DataSensitivity::Plain,
        llm_confidence: Some(0.5),
        accumulated_risk: base.accumulated_risk,
    }
}

/// Push a WritePersist into the HumanConfirm band (50..90).
/// WritePersist(40) * Neutral(2) * Plain(1) = 80 -> 50..90.
fn home_write_ctx(base: &RiskContext) -> RiskContext {
    RiskContext {
        trust_level: TrustLevel::Neutral,
        data_sensitivity: DataSensitivity::Plain,
        llm_confidence: Some(1.0),
        accumulated_risk: base.accumulated_risk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::risk::RiskDecision;
    use std::sync::Mutex;

    struct MockGrants {
        granted: Mutex<Vec<(Uuid, std::path::PathBuf, bool)>>,
    }

    impl MockGrants {
        fn new() -> Self {
            Self {
                granted: Mutex::new(Vec::new()),
            }
        }
        fn grant(&self, arc: Uuid, path: &Path, write: bool) {
            self.granted
                .lock()
                .unwrap()
                .push((arc, path.to_path_buf(), write));
        }
    }

    #[async_trait]
    impl GrantLookup for MockGrants {
        async fn check(&self, arc_id: Uuid, path: &Path, write: bool) -> Result<bool> {
            let g = self.granted.lock().unwrap();
            Ok(g.iter().any(|(a, p, w)| {
                *a == arc_id
                    && paths::path_within(path, p)
                    && (*w || !write)
            }))
        }
    }

    fn ctx() -> RiskContext {
        RiskContext {
            trust_level: TrustLevel::AuthUser,
            data_sensitivity: DataSensitivity::Plain,
            llm_confidence: Some(1.0),
            accumulated_risk: 0,
        }
    }

    #[tokio::test]
    async fn system_write_blocked_regardless_of_grants() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        grants.grant(arc, Path::new("/etc"), true);

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, Path::new("/etc/passwd"), PathAccess::Write, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::HardBlock);
    }

    #[tokio::test]
    async fn granted_write_is_safe() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        let target = std::env::temp_dir().join("athen_eval_granted");
        grants.grant(arc, &target, true);

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, &target, PathAccess::Write, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::SilentApprove);
    }

    #[tokio::test]
    async fn home_write_without_grant_requires_confirm() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        let home = paths::home_dir().expect("home dir");
        let target = home.join("Documents").join("draft.txt");

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, &target, PathAccess::Write, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::HumanConfirm);
    }

    #[tokio::test]
    async fn home_read_without_grant_is_caution() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        let home = paths::home_dir().expect("home dir");
        let target = home.join("Documents").join("notes.md");

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, &target, PathAccess::Read, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::NotifyAndProceed);
    }

    #[tokio::test]
    async fn athen_data_dir_is_safe() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        let data = paths::athen_data_dir().expect("data dir");
        let target = data.join("files").join("note.txt");

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, &target, PathAccess::Write, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::SilentApprove);
    }

    #[tokio::test]
    async fn outside_home_write_requires_confirm() {
        let grants = MockGrants::new();
        let arc = Uuid::new_v4();
        // /opt is not in the system list nor under $HOME on Linux.
        let target = Path::new("/opt/some-app/data/foo.txt");

        let eval = PathRiskEvaluator::new(grants);
        let score = eval
            .evaluate(arc, target, PathAccess::Write, &ctx())
            .await
            .unwrap();

        assert_eq!(score.decision(), RiskDecision::HumanConfirm);
    }
}
