//! Agent pool dispatch logic.
//!
//! Manages a pool of available agents and assigns them to tasks.

use std::collections::HashMap;

use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::task::{AgentId, Task, TaskId};

/// Manages agent availability and task assignments.
pub struct Dispatcher {
    available_agents: Mutex<Vec<AgentId>>,
    assigned: Mutex<HashMap<TaskId, AgentId>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            available_agents: Mutex::new(Vec::new()),
            assigned: Mutex::new(HashMap::new()),
        }
    }

    /// Add an agent to the available pool.
    pub async fn register_agent(&self, id: AgentId) {
        let mut agents = self.available_agents.lock().await;
        if !agents.contains(&id) {
            agents.push(id);
        }
    }

    /// Remove an agent from both available and assigned pools.
    pub async fn unregister_agent(&self, id: AgentId) {
        let mut agents = self.available_agents.lock().await;
        agents.retain(|a| *a != id);

        let mut assigned = self.assigned.lock().await;
        assigned.retain(|_, agent| *agent != id);
    }

    /// Take an available agent and assign it to the given task.
    /// Returns `None` if no agents are available.
    pub async fn assign_task(&self, task: &Task) -> Option<AgentId> {
        let mut agents = self.available_agents.lock().await;
        if agents.is_empty() {
            return None;
        }

        let agent_id = agents.remove(0);
        let mut assigned = self.assigned.lock().await;
        assigned.insert(task.id, agent_id);

        Some(agent_id)
    }

    /// Release an agent back to the available pool when a task completes.
    pub async fn release_agent(&self, task_id: TaskId) -> Result<()> {
        let mut assigned = self.assigned.lock().await;
        match assigned.remove(&task_id) {
            Some(agent_id) => {
                drop(assigned);
                let mut agents = self.available_agents.lock().await;
                agents.push(agent_id);
                Ok(())
            }
            None => Err(AthenError::TaskNotFound(task_id.to_string())),
        }
    }

    /// Look up which agent is assigned to a task.
    pub async fn assigned_agent(&self, task_id: TaskId) -> Option<AgentId> {
        let assigned = self.assigned.lock().await;
        assigned.get(&task_id).copied()
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_task() -> Task {
        let now = Utc::now();
        Task {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            source_event: None,
            domain: athen_core::task::DomainType::Base,
            description: "Test task".to_string(),
            priority: athen_core::task::TaskPriority::Normal,
            status: athen_core::task::TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: Vec::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn test_assign_release_cycle() {
        let dispatcher = Dispatcher::new();
        let agent_id = Uuid::new_v4();
        let task = make_task();
        let task_id = task.id;

        dispatcher.register_agent(agent_id).await;

        // Assign
        let assigned = dispatcher.assign_task(&task).await;
        assert_eq!(assigned, Some(agent_id));

        // Verify assignment
        assert_eq!(dispatcher.assigned_agent(task_id).await, Some(agent_id));

        // Release
        dispatcher.release_agent(task_id).await.unwrap();

        // Agent should be available again
        assert_eq!(dispatcher.assigned_agent(task_id).await, None);

        // Can assign again
        let task2 = make_task();
        let assigned2 = dispatcher.assign_task(&task2).await;
        assert_eq!(assigned2, Some(agent_id));
    }

    #[tokio::test]
    async fn test_no_agents_available_returns_none() {
        let dispatcher = Dispatcher::new();
        let task = make_task();

        let result = dispatcher.assign_task(&task).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_register_duplicate_agent() {
        let dispatcher = Dispatcher::new();
        let agent_id = Uuid::new_v4();

        dispatcher.register_agent(agent_id).await;
        dispatcher.register_agent(agent_id).await;

        // Should only have one entry
        let task1 = make_task();
        let task2 = make_task();

        dispatcher.assign_task(&task1).await;
        let result = dispatcher.assign_task(&task2).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_unregister_agent() {
        let dispatcher = Dispatcher::new();
        let agent_id = Uuid::new_v4();

        dispatcher.register_agent(agent_id).await;
        dispatcher.unregister_agent(agent_id).await;

        let task = make_task();
        let result = dispatcher.assign_task(&task).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_release_unknown_task_returns_error() {
        let dispatcher = Dispatcher::new();
        let result = dispatcher.release_agent(Uuid::new_v4()).await;
        assert!(result.is_err());
    }
}
