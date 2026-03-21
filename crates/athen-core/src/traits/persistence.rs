use async_trait::async_trait;

use crate::error::Result;
use crate::ipc::IpcMessage;
use crate::task::{Task, TaskId, TaskStatus};

/// Persistent storage for tasks and operational state.
#[async_trait]
pub trait PersistentStore: Send + Sync {
    async fn save_task(&self, task: &Task) -> Result<()>;
    async fn load_task(&self, id: TaskId) -> Result<Option<Task>>;
    async fn list_tasks(&self, filter: TaskFilter) -> Result<Vec<Task>>;

    async fn save_checkpoint(&self, task_id: TaskId, data: serde_json::Value) -> Result<()>;
    async fn load_checkpoint(&self, task_id: TaskId) -> Result<Option<serde_json::Value>>;

    async fn save_pending_message(&self, msg: &IpcMessage) -> Result<()>;
    async fn pop_pending_messages(&self, limit: usize) -> Result<Vec<IpcMessage>>;
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub limit: Option<usize>,
}
