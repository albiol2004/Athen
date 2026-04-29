//! Priority task queue implementation.
//!
//! Uses a `BinaryHeap` to order tasks by priority (highest first),
//! breaking ties by creation time (oldest first).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::task::{Task, TaskId, TaskPriority, TaskStatus};
use athen_core::traits::coordinator::TaskQueue;

/// A task wrapper that provides ordering for the binary heap.
struct PrioritizedTask {
    priority: TaskPriority,
    created_at: DateTime<Utc>,
    task: Task,
}

impl PartialEq for PrioritizedTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.created_at == other.created_at
    }
}

impl Eq for PrioritizedTask {}

impl PartialOrd for PrioritizedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first
        self.priority
            .cmp(&other.priority)
            // Within same priority, older tasks first (reverse chronological)
            .then_with(|| other.created_at.cmp(&self.created_at))
    }
}

/// A priority-based task queue backed by a `BinaryHeap`.
pub struct PriorityTaskQueue {
    tasks: Mutex<BinaryHeap<PrioritizedTask>>,
}

impl PriorityTaskQueue {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(BinaryHeap::new()),
        }
    }
}

impl Default for PriorityTaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskQueue for PriorityTaskQueue {
    async fn enqueue(&self, task: Task) -> Result<TaskId> {
        let id = task.id;
        let prioritized = PrioritizedTask {
            priority: task.priority,
            created_at: task.created_at,
            task,
        };
        let mut heap = self.tasks.lock().await;
        heap.push(prioritized);
        Ok(id)
    }

    async fn dequeue(&self) -> Result<Option<Task>> {
        let mut heap = self.tasks.lock().await;
        Ok(heap.pop().map(|pt| pt.task))
    }

    async fn update_status(&self, id: TaskId, status: TaskStatus) -> Result<()> {
        let mut heap = self.tasks.lock().await;
        let mut found = false;

        // Drain the heap, update the matching task, rebuild
        let mut items: Vec<PrioritizedTask> = std::iter::from_fn(|| heap.pop()).collect();
        for item in &mut items {
            if item.task.id == id {
                item.task.status = status;
                item.task.updated_at = Utc::now();
                found = true;
            }
        }

        for item in items {
            heap.push(item);
        }

        if found {
            Ok(())
        } else {
            Err(AthenError::TaskNotFound(id.to_string()))
        }
    }

    async fn pending_count(&self) -> Result<usize> {
        let heap = self.tasks.lock().await;
        Ok(heap.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn make_task(priority: TaskPriority, created_at: DateTime<Utc>) -> Task {
        Task {
            id: Uuid::new_v4(),
            created_at,
            updated_at: created_at,
            source_event: None,
            domain: athen_core::task::DomainType::Base,
            description: format!("Task with priority {:?}", priority),
            priority,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: Vec::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn test_priority_ordering_critical_before_normal() {
        let queue = PriorityTaskQueue::new();
        let now = Utc::now();

        let normal = make_task(TaskPriority::Normal, now);
        let critical = make_task(TaskPriority::Critical, now);
        let low = make_task(TaskPriority::Low, now);

        let normal_id = normal.id;
        let critical_id = critical.id;
        let low_id = low.id;

        queue.enqueue(normal).await.unwrap();
        queue.enqueue(critical).await.unwrap();
        queue.enqueue(low).await.unwrap();

        let first = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(first.id, critical_id);

        let second = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(second.id, normal_id);

        let third = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(third.id, low_id);
    }

    #[tokio::test]
    async fn test_fifo_within_same_priority() {
        let queue = PriorityTaskQueue::new();
        let now = Utc::now();

        let first_task = make_task(TaskPriority::Normal, now - Duration::seconds(10));
        let second_task = make_task(TaskPriority::Normal, now);

        let first_id = first_task.id;
        let second_id = second_task.id;

        // Enqueue second first to ensure ordering is by created_at, not insertion order
        queue.enqueue(second_task).await.unwrap();
        queue.enqueue(first_task).await.unwrap();

        let dequeued_first = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(dequeued_first.id, first_id);

        let dequeued_second = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(dequeued_second.id, second_id);
    }

    #[tokio::test]
    async fn test_dequeue_empty_returns_none() {
        let queue = PriorityTaskQueue::new();
        let result = queue.dequeue().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_pending_count() {
        let queue = PriorityTaskQueue::new();
        let now = Utc::now();

        assert_eq!(queue.pending_count().await.unwrap(), 0);

        queue
            .enqueue(make_task(TaskPriority::Normal, now))
            .await
            .unwrap();
        queue
            .enqueue(make_task(TaskPriority::High, now))
            .await
            .unwrap();

        assert_eq!(queue.pending_count().await.unwrap(), 2);

        queue.dequeue().await.unwrap();
        assert_eq!(queue.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_update_status() {
        let queue = PriorityTaskQueue::new();
        let now = Utc::now();

        let task = make_task(TaskPriority::Normal, now);
        let task_id = task.id;

        queue.enqueue(task).await.unwrap();
        queue
            .update_status(task_id, TaskStatus::InProgress)
            .await
            .unwrap();

        let dequeued = queue.dequeue().await.unwrap().unwrap();
        assert_eq!(dequeued.id, task_id);
        assert_eq!(dequeued.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn test_update_status_not_found() {
        let queue = PriorityTaskQueue::new();
        let result = queue
            .update_status(Uuid::new_v4(), TaskStatus::InProgress)
            .await;
        assert!(result.is_err());
    }
}
