use async_trait::async_trait;

use crate::error::Result;
use crate::event::SenseEvent;
use crate::risk::{RiskContext, RiskScore};
use crate::task::{Task, TaskId, TaskStatus};

/// Routes incoming SenseEvents to the appropriate processing pipeline.
#[async_trait]
pub trait EventRouter: Send + Sync {
    /// Classify and route an event. Returns the task(s) to create.
    async fn route(&self, event: SenseEvent) -> Result<Vec<Task>>;
}

/// Evaluates risk for a proposed action or task.
#[async_trait]
pub trait RiskEvaluator: Send + Sync {
    /// Compute risk score for a task in a given context.
    async fn evaluate(&self, task: &Task, context: &RiskContext) -> Result<RiskScore>;

    /// Whether this task requires human approval given its risk score.
    fn requires_approval(&self, score: &RiskScore) -> bool;
}

/// Priority queue for tasks awaiting agent execution.
#[async_trait]
pub trait TaskQueue: Send + Sync {
    async fn enqueue(&self, task: Task) -> Result<TaskId>;
    async fn dequeue(&self) -> Result<Option<Task>>;
    async fn update_status(&self, id: TaskId, status: TaskStatus) -> Result<()>;
    async fn pending_count(&self) -> Result<usize>;
}
