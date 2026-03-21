//! Step-by-step audit logging.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use athen_core::error::Result;
use athen_core::task::{TaskId, TaskStep};
use athen_core::traits::agent::StepAuditor;

/// In-memory auditor that stores steps per task.
///
/// Thread-safe via `tokio::sync::Mutex`. Suitable for testing and
/// short-lived agent processes. For persistent auditing across restarts,
/// a database-backed implementation should be used instead.
pub struct InMemoryAuditor {
    steps: Mutex<HashMap<TaskId, Vec<TaskStep>>>,
}

impl InMemoryAuditor {
    /// Create a new empty auditor.
    pub fn new() -> Self {
        Self {
            steps: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryAuditor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StepAuditor for InMemoryAuditor {
    async fn record_step(&self, task_id: TaskId, step: &TaskStep) -> Result<()> {
        let mut steps = self.steps.lock().await;
        steps.entry(task_id).or_default().push(step.clone());
        tracing::debug!(
            task_id = %task_id,
            step_index = step.index,
            description = %step.description,
            "Recorded step"
        );
        Ok(())
    }

    async fn get_steps(&self, task_id: TaskId) -> Result<Vec<TaskStep>> {
        let steps = self.steps.lock().await;
        Ok(steps.get(&task_id).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::task::StepStatus;
    use uuid::Uuid;

    fn make_step(index: u32, description: &str) -> TaskStep {
        TaskStep {
            id: Uuid::new_v4(),
            index,
            description: description.to_string(),
            status: StepStatus::Completed,
            started_at: None,
            completed_at: None,
            output: None,
            checkpoint: None,
        }
    }

    #[tokio::test]
    async fn test_record_and_get_steps() {
        let auditor = InMemoryAuditor::new();
        let task_id = Uuid::new_v4();

        let step1 = make_step(0, "First step");
        let step2 = make_step(1, "Second step");

        auditor.record_step(task_id, &step1).await.unwrap();
        auditor.record_step(task_id, &step2).await.unwrap();

        let steps = auditor.get_steps(task_id).await.unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].description, "First step");
        assert_eq!(steps[1].description, "Second step");
    }

    #[tokio::test]
    async fn test_get_steps_unknown_task() {
        let auditor = InMemoryAuditor::new();
        let steps = auditor.get_steps(Uuid::new_v4()).await.unwrap();
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn test_separate_tasks() {
        let auditor = InMemoryAuditor::new();
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();

        auditor
            .record_step(task_a, &make_step(0, "A step"))
            .await
            .unwrap();
        auditor
            .record_step(task_b, &make_step(0, "B step"))
            .await
            .unwrap();

        let steps_a = auditor.get_steps(task_a).await.unwrap();
        let steps_b = auditor.get_steps(task_b).await.unwrap();
        assert_eq!(steps_a.len(), 1);
        assert_eq!(steps_b.len(), 1);
        assert_eq!(steps_a[0].description, "A step");
        assert_eq!(steps_b[0].description, "B step");
    }
}
