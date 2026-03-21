use async_trait::async_trait;

use crate::error::Result;
use crate::task::{Task, TaskId, TaskStep};

/// Executes a task through LLM-driven steps, calling tools as needed.
#[async_trait]
pub trait AgentExecutor: Send + Sync {
    /// Execute a task to completion or failure.
    async fn execute(&self, task: Task) -> Result<TaskResult>;
}

/// Records each step an agent takes for audit and replay.
#[async_trait]
pub trait StepAuditor: Send + Sync {
    async fn record_step(&self, task_id: TaskId, step: &TaskStep) -> Result<()>;
    async fn get_steps(&self, task_id: TaskId) -> Result<Vec<TaskStep>>;
}

/// Guards against runaway execution.
pub trait TimeoutGuard: Send + Sync {
    fn remaining(&self) -> std::time::Duration;
    fn is_expired(&self) -> bool;
}

/// Monitors resource consumption of an agent process.
#[async_trait]
pub trait ResourceMonitor: Send + Sync {
    async fn current_usage(&self) -> Result<ResourceUsage>;
    fn is_within_limits(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct TaskResult {
    pub task_id: TaskId,
    pub success: bool,
    pub output: Option<serde_json::Value>,
    pub steps_completed: u32,
    pub total_risk_used: u32,
}

#[derive(Debug, Clone)]
pub struct ResourceUsage {
    pub memory_bytes: u64,
    pub cpu_percent: f32,
}
