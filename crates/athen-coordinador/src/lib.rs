//! Coordinator process for Athen.
//!
//! Receives events from monitors, evaluates risk, prioritizes,
//! and dispatches tasks to agent workers.

pub mod dispatcher;
pub mod queue;
pub mod risk;
pub mod router;

use athen_core::contact::TrustLevel;
use athen_core::error::Result;
use athen_core::event::SenseEvent;
use athen_core::risk::{DataSensitivity, RiskContext, RiskDecision};
use athen_core::task::{AgentId, TaskId, TaskStatus};
use athen_core::traits::coordinator::{EventRouter, RiskEvaluator, TaskQueue};

use crate::dispatcher::Dispatcher;
use crate::queue::PriorityTaskQueue;
use crate::risk::CoordinatorRiskEvaluator;
use crate::router::DefaultRouter;

/// The main coordinator that orchestrates event processing, risk evaluation,
/// queueing, and agent dispatch.
pub struct Coordinator {
    router: DefaultRouter,
    queue: PriorityTaskQueue,
    dispatcher: Dispatcher,
    risk_evaluator: CoordinatorRiskEvaluator,
}

impl Coordinator {
    pub fn new(risk_evaluator: Box<dyn RiskEvaluator>) -> Self {
        Self {
            router: DefaultRouter::new(),
            queue: PriorityTaskQueue::new(),
            dispatcher: Dispatcher::new(),
            risk_evaluator: CoordinatorRiskEvaluator::new(risk_evaluator),
        }
    }

    /// Process an incoming sense event end-to-end.
    ///
    /// 1. Route event to tasks
    /// 2. Evaluate risk for each task
    /// 3. Set status based on risk decision
    /// 4. Enqueue tasks that can proceed
    /// 5. Return created task IDs
    pub async fn process_event(&self, event: SenseEvent) -> Result<Vec<TaskId>> {
        let mut tasks = self.router.route(event).await?;
        let mut task_ids = Vec::with_capacity(tasks.len());

        for task in &mut tasks {
            // Build a default risk context. In a full implementation this would
            // be derived from the sender's contact trust level and content analysis.
            let context = RiskContext {
                trust_level: TrustLevel::Neutral,
                data_sensitivity: DataSensitivity::Plain,
                llm_confidence: None,
                accumulated_risk: 0,
            };

            let decision = self
                .risk_evaluator
                .evaluate_and_decide(task, &context)
                .await?;

            // Also store the full risk score on the task
            let score = self.risk_evaluator.evaluate(task, &context).await?;
            task.risk_score = Some(score);

            match decision {
                RiskDecision::SilentApprove | RiskDecision::NotifyAndProceed => {
                    task.status = TaskStatus::Pending;
                }
                RiskDecision::HumanConfirm => {
                    task.status = TaskStatus::AwaitingApproval;
                }
                RiskDecision::HardBlock => {
                    task.status = TaskStatus::Cancelled;
                }
            }

            task_ids.push(task.id);

            // Only enqueue tasks that can proceed
            if task.status == TaskStatus::Pending {
                self.queue.enqueue(task.clone()).await?;
            }
        }

        Ok(task_ids)
    }

    /// Dispatch the next task from the queue to an available agent.
    pub async fn dispatch_next(&self) -> Result<Option<(TaskId, AgentId)>> {
        let task = match self.queue.dequeue().await? {
            Some(t) => t,
            None => return Ok(None),
        };

        match self.dispatcher.assign_task(&task).await {
            Some(agent_id) => Ok(Some((task.id, agent_id))),
            None => {
                // No agent available; re-enqueue the task
                self.queue.enqueue(task).await?;
                Ok(None)
            }
        }
    }

    /// Handle task completion: release the assigned agent.
    pub async fn complete_task(&self, task_id: TaskId) -> Result<()> {
        self.dispatcher.release_agent(task_id).await
    }

    /// Access the dispatcher for agent registration.
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// Access the queue for inspection.
    pub fn queue(&self) -> &PriorityTaskQueue {
        &self.queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use athen_core::event::{EventKind, EventSource, NormalizedContent};
    use athen_core::risk::{EvaluationMethod, RiskLevel, RiskScore};
    use athen_core::task::Task;
    use chrono::Utc;
    use uuid::Uuid;

    /// Mock risk evaluator that returns a configurable risk score.
    struct MockRiskEvaluator {
        total_score: f64,
    }

    impl MockRiskEvaluator {
        fn new(total_score: f64) -> Self {
            Self { total_score }
        }
    }

    #[async_trait]
    impl RiskEvaluator for MockRiskEvaluator {
        async fn evaluate(&self, _task: &Task, _context: &RiskContext) -> Result<RiskScore> {
            Ok(RiskScore {
                total: self.total_score,
                base_impact: 1.0,
                origin_multiplier: 1.0,
                data_multiplier: 1.0,
                uncertainty_penalty: 0.0,
                level: if self.total_score < 20.0 {
                    RiskLevel::Safe
                } else if self.total_score < 50.0 {
                    RiskLevel::Caution
                } else if self.total_score < 90.0 {
                    RiskLevel::Danger
                } else {
                    RiskLevel::Critical
                },
                evaluation_method: EvaluationMethod::RuleBased,
            })
        }

        fn requires_approval(&self, score: &RiskScore) -> bool {
            score.total >= 50.0
        }
    }

    fn make_event(source: EventSource) -> SenseEvent {
        SenseEvent {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            source,
            kind: EventKind::NewMessage,
            sender: None,
            content: NormalizedContent {
                summary: Some("Test event".to_string()),
                body: serde_json::Value::Null,
                attachments: Vec::new(),
            },
            source_risk: RiskLevel::Safe,
            raw_id: None,
        }
    }

    #[tokio::test]
    async fn test_process_event_low_risk() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));

        let event = make_event(EventSource::UserInput);
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        // Task should be enqueued (low risk = Pending)
        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_process_event_high_risk_awaiting_approval() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(60.0)));

        let event = make_event(EventSource::Email);
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        // High risk task should NOT be enqueued (AwaitingApproval)
        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_process_event_hard_block() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(95.0)));

        let event = make_event(EventSource::System);
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        // Hard-blocked task should NOT be enqueued
        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_dispatch_next_with_agent() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));
        let agent_id = Uuid::new_v4();

        coordinator.dispatcher.register_agent(agent_id).await;

        let event = make_event(EventSource::UserInput);
        coordinator.process_event(event).await.unwrap();

        let result = coordinator.dispatch_next().await.unwrap();
        assert!(result.is_some());

        let (task_id, dispatched_agent) = result.unwrap();
        assert_eq!(dispatched_agent, agent_id);

        // Complete the task
        coordinator.complete_task(task_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_next_no_agent() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));

        let event = make_event(EventSource::UserInput);
        coordinator.process_event(event).await.unwrap();

        // No agents registered
        let result = coordinator.dispatch_next().await.unwrap();
        assert!(result.is_none());

        // Task should be re-enqueued
        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_dispatch_next_empty_queue() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));

        let result = coordinator.dispatch_next().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_full_flow() {
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(10.0)));
        let agent1 = Uuid::new_v4();
        let agent2 = Uuid::new_v4();

        coordinator.dispatcher.register_agent(agent1).await;
        coordinator.dispatcher.register_agent(agent2).await;

        // Process two events
        let event1 = make_event(EventSource::Calendar);
        let event2 = make_event(EventSource::Email);

        coordinator.process_event(event1).await.unwrap();
        coordinator.process_event(event2).await.unwrap();

        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 2);

        // Dispatch both
        let (tid1, _) = coordinator.dispatch_next().await.unwrap().unwrap();
        let (tid2, _) = coordinator.dispatch_next().await.unwrap().unwrap();

        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 0);

        // Complete both
        coordinator.complete_task(tid1).await.unwrap();
        coordinator.complete_task(tid2).await.unwrap();
    }
}
