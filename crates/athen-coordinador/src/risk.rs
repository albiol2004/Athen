//! Risk evaluation integration with the coordinator.
//!
//! Wraps a `RiskEvaluator` trait object and provides decision-making logic.

use athen_core::error::Result;
use athen_core::risk::{RiskContext, RiskDecision, RiskScore};
use athen_core::task::Task;
use athen_core::traits::coordinator::RiskEvaluator;

/// Coordinator-level risk evaluator that wraps a `RiskEvaluator` implementation
/// and provides high-level decision-making.
pub struct CoordinatorRiskEvaluator {
    evaluator: Box<dyn RiskEvaluator>,
}

impl CoordinatorRiskEvaluator {
    pub fn new(evaluator: Box<dyn RiskEvaluator>) -> Self {
        Self { evaluator }
    }

    /// Evaluate a task's risk and return the decision.
    pub async fn evaluate_and_decide(
        &self,
        task: &Task,
        context: &RiskContext,
    ) -> Result<RiskDecision> {
        let score = self.evaluator.evaluate(task, context).await?;
        Ok(score.decision())
    }

    /// Get the full risk score for a task.
    pub async fn evaluate(&self, task: &Task, context: &RiskContext) -> Result<RiskScore> {
        self.evaluator.evaluate(task, context).await
    }

    /// Check whether a score requires human approval.
    pub fn requires_approval(&self, score: &RiskScore) -> bool {
        self.evaluator.requires_approval(score)
    }
}
