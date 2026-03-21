//! Coordinator process for Athen.
//!
//! Receives events from monitors, evaluates risk, prioritizes,
//! and dispatches tasks to agent workers.

pub mod dispatcher;
pub mod queue;
pub mod risk;
pub mod router;

use std::collections::HashMap;

use athen_contacts::trust::TrustManager;
use athen_core::contact::{ContactId, IdentifierKind, TrustLevel};
use athen_core::error::Result;
use athen_core::event::{EventSource, SenseEvent, SenderInfo};
use athen_core::risk::{DataSensitivity, RiskContext, RiskDecision};
use athen_core::task::{AgentId, TaskId, TaskStatus};
use athen_core::traits::coordinator::{EventRouter, RiskEvaluator, TaskQueue};
use athen_core::traits::persistence::{PersistentStore, TaskFilter};
use tokio::sync::Mutex;

/// Infer the `IdentifierKind` from a sender identifier string.
fn infer_identifier_kind(identifier: &str) -> IdentifierKind {
    if identifier.contains('@') {
        IdentifierKind::Email
    } else if identifier.len() >= 7
        && identifier
            .chars()
            .all(|c| c.is_ascii_digit() || c == '+' || c == '-' || c == ' ')
    {
        IdentifierKind::Phone
    } else {
        IdentifierKind::Other
    }
}

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
    store: Option<Box<dyn PersistentStore>>,
    trust_manager: Option<TrustManager>,
    /// Maps task IDs to resolved contact IDs for trust feedback on completion.
    task_contacts: Mutex<HashMap<TaskId, ContactId>>,
}

impl Coordinator {
    pub fn new(risk_evaluator: Box<dyn RiskEvaluator>) -> Self {
        Self {
            router: DefaultRouter::new(),
            queue: PriorityTaskQueue::new(),
            dispatcher: Dispatcher::new(),
            risk_evaluator: CoordinatorRiskEvaluator::new(risk_evaluator),
            store: None,
            trust_manager: None,
            task_contacts: Mutex::new(HashMap::new()),
        }
    }

    /// Attach a persistent store for task durability.
    /// Without this, tasks only live in memory.
    pub fn with_persistence(mut self, store: Box<dyn PersistentStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Add a `TrustManager` for contact-aware risk evaluation.
    ///
    /// When configured, the coordinator resolves sender trust levels from
    /// the contact store and uses them in risk evaluation instead of the
    /// default `TrustLevel::Neutral`. Completed tasks also record a
    /// positive trust interaction for the originating contact.
    pub fn with_trust_manager(mut self, tm: TrustManager) -> Self {
        self.trust_manager = Some(tm);
        self
    }

    /// Resolve a sender's trust level via the TrustManager.
    ///
    /// Returns the trust level and the resolved contact ID.
    /// Falls back to `TrustLevel::Neutral` when no trust manager is configured.
    async fn resolve_sender_trust(
        &self,
        sender: &SenderInfo,
    ) -> (TrustLevel, Option<ContactId>) {
        let Some(ref tm) = self.trust_manager else {
            return (TrustLevel::Neutral, None);
        };

        let kind = infer_identifier_kind(&sender.identifier);

        match tm.resolve_contact(&sender.identifier, kind).await {
            Ok(contact) => {
                let trust = if contact.blocked {
                    TrustLevel::Unknown
                } else {
                    contact.trust_level
                };
                (trust, Some(contact.id))
            }
            Err(_) => (TrustLevel::Neutral, None),
        }
    }

    /// Process an incoming sense event end-to-end.
    ///
    /// 1. Resolve sender trust level (if sender present)
    /// 2. Route event to tasks
    /// 3. Evaluate risk for each task (using resolved trust)
    /// 4. Set status based on risk decision
    /// 5. Enqueue tasks that can proceed
    /// 6. Return created task IDs
    pub async fn process_event(&self, event: SenseEvent) -> Result<Vec<TaskId>> {
        // Resolve trust level from the sender, if present and not a UserInput event.
        let (trust_level, contact_id) = match (&event.sender, &event.source) {
            (Some(sender), source) if *source != EventSource::UserInput => {
                self.resolve_sender_trust(sender).await
            }
            // UserInput events come from the authenticated user.
            (_, EventSource::UserInput) => (TrustLevel::AuthUser, None),
            // No sender information available.
            _ => (TrustLevel::Neutral, None),
        };

        let mut tasks = self.router.route(event).await?;
        let mut task_ids = Vec::with_capacity(tasks.len());

        for task in &mut tasks {
            let context = RiskContext {
                trust_level,
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

            // Track contact for trust feedback on task completion.
            if let Some(cid) = contact_id {
                self.task_contacts.lock().await.insert(task.id, cid);
            }

            // Only enqueue tasks that can proceed
            if task.status == TaskStatus::Pending {
                self.queue.enqueue(task.clone()).await?;
            }

            // Persist the task if a store is available.
            // Persistence errors are logged but do not fail the operation.
            if let Some(ref store) = self.store {
                if let Err(e) = store.save_task(task).await {
                    tracing::warn!(task_id = %task.id, error = %e, "Failed to persist task");
                }
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

    /// Handle task completion: release the assigned agent and record
    /// a positive trust interaction for the originating contact.
    pub async fn complete_task(&self, task_id: TaskId) -> Result<()> {
        self.dispatcher.release_agent(task_id).await?;

        // Update the task status in the persistent store if available.
        if let Some(ref store) = self.store {
            match store.load_task(task_id).await {
                Ok(Some(mut task)) => {
                    task.status = TaskStatus::Completed;
                    task.updated_at = chrono::Utc::now();
                    if let Err(e) = store.save_task(&task).await {
                        tracing::warn!(task_id = %task_id, error = %e, "Failed to persist completed task");
                    }
                }
                Ok(None) => {
                    tracing::debug!(task_id = %task_id, "Task not found in store for completion update");
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "Failed to load task for completion update");
                }
            }
        }

        // Record approval for the contact associated with this task.
        if let Some(ref tm) = self.trust_manager {
            let contact_id = self.task_contacts.lock().await.remove(&task_id);
            if let Some(cid) = contact_id {
                // Best-effort: log but don't fail the completion if trust update fails.
                if let Err(e) = tm.record_approval(cid).await {
                    tracing::warn!(
                        task_id = %task_id,
                        contact_id = %cid,
                        error = %e,
                        "Failed to record approval for contact trust evolution"
                    );
                }
            }
        }

        Ok(())
    }

    /// Recover non-terminal tasks from the persistent store and re-enqueue them.
    ///
    /// Call this at startup to resume work that was interrupted.
    /// Terminal statuses (Completed, Failed, Cancelled) are skipped.
    /// Returns the number of tasks recovered, or 0 if no store is configured.
    pub async fn recover_tasks(&self) -> Result<usize> {
        let store = match self.store {
            Some(ref s) => s,
            None => return Ok(0),
        };

        // Load all tasks without a status filter, then filter in-memory
        // for non-terminal statuses.
        let all_tasks = store.list_tasks(TaskFilter::default()).await?;

        let mut recovered = 0;
        for task in all_tasks {
            match task.status {
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
                    continue;
                }
                _ => {}
            }

            // Re-enqueue tasks that were pending or in progress.
            // AwaitingApproval and Paused tasks are also recovered so they
            // are not lost, but they keep their original status.
            if task.status == TaskStatus::Pending || task.status == TaskStatus::InProgress {
                if let Err(e) = self.queue.enqueue(task).await {
                    tracing::warn!(error = %e, "Failed to re-enqueue recovered task");
                    continue;
                }
            }
            recovered += 1;
        }

        tracing::info!(count = recovered, "Recovered tasks from persistent store");
        Ok(recovered)
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
    use athen_contacts::InMemoryContactStore;
    use athen_core::event::{EventKind, EventSource, NormalizedContent, SenderInfo};
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

    fn make_event_with_sender(source: EventSource, sender_id: &str) -> SenseEvent {
        SenseEvent {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            source,
            kind: EventKind::NewMessage,
            sender: Some(SenderInfo {
                identifier: sender_id.to_string(),
                contact_id: None,
                display_name: None,
            }),
            content: NormalizedContent {
                summary: Some("Test event".to_string()),
                body: serde_json::Value::Null,
                attachments: Vec::new(),
            },
            source_risk: RiskLevel::Safe,
            raw_id: None,
        }
    }

    fn make_trust_manager() -> TrustManager {
        TrustManager::new(Box::new(InMemoryContactStore::new()))
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

    // --- Trust integration tests ---

    #[tokio::test]
    async fn test_process_event_without_trust_manager_uses_neutral() {
        // Without a trust manager, events with senders still work (default Neutral).
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));

        let event = make_event_with_sender(EventSource::Email, "alice@example.com");
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        assert_eq!(coordinator.queue.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_process_event_with_trust_manager_resolves_sender() {
        let tm = make_trust_manager();
        let coordinator =
            Coordinator::new(Box::new(MockRiskEvaluator::new(5.0))).with_trust_manager(tm);

        let event = make_event_with_sender(EventSource::Email, "new@example.com");
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        // Task should be tracked for trust feedback.
        let contacts = coordinator.task_contacts.lock().await;
        assert!(contacts.contains_key(&task_ids[0]));
    }

    #[tokio::test]
    async fn test_user_input_uses_auth_user_trust() {
        // UserInput events should use AuthUser trust, not look up the sender.
        let tm = make_trust_manager();
        let coordinator =
            Coordinator::new(Box::new(MockRiskEvaluator::new(5.0))).with_trust_manager(tm);

        // Even with a sender, UserInput should skip trust lookup.
        let event = make_event_with_sender(EventSource::UserInput, "user@local");
        let task_ids = coordinator.process_event(event).await.unwrap();

        assert_eq!(task_ids.len(), 1);
        // No contact should be tracked for UserInput events.
        let contacts = coordinator.task_contacts.lock().await;
        assert!(!contacts.contains_key(&task_ids[0]));
    }

    #[tokio::test]
    async fn test_complete_task_records_approval() {
        let tm = make_trust_manager();
        let coordinator =
            Coordinator::new(Box::new(MockRiskEvaluator::new(5.0))).with_trust_manager(tm);
        let agent_id = Uuid::new_v4();

        coordinator.dispatcher.register_agent(agent_id).await;

        let event = make_event_with_sender(EventSource::Email, "sender@example.com");
        coordinator.process_event(event).await.unwrap();

        let (task_id, _) = coordinator.dispatch_next().await.unwrap().unwrap();
        coordinator.complete_task(task_id).await.unwrap();

        // The contact mapping should be removed after completion.
        let contacts = coordinator.task_contacts.lock().await;
        assert!(!contacts.contains_key(&task_id));
    }

    #[tokio::test]
    async fn test_complete_task_without_trust_manager_still_works() {
        // Backward compat: complete_task works without trust manager.
        let coordinator = Coordinator::new(Box::new(MockRiskEvaluator::new(5.0)));
        let agent_id = Uuid::new_v4();

        coordinator.dispatcher.register_agent(agent_id).await;

        let event = make_event(EventSource::UserInput);
        coordinator.process_event(event).await.unwrap();

        let (task_id, _) = coordinator.dispatch_next().await.unwrap().unwrap();
        coordinator.complete_task(task_id).await.unwrap();
    }

    #[test]
    fn test_infer_identifier_kind_email() {
        assert_eq!(
            infer_identifier_kind("user@example.com"),
            IdentifierKind::Email
        );
    }

    #[test]
    fn test_infer_identifier_kind_phone() {
        assert_eq!(
            infer_identifier_kind("+1-555-1234567"),
            IdentifierKind::Phone
        );
    }

    #[test]
    fn test_infer_identifier_kind_other() {
        assert_eq!(infer_identifier_kind("johndoe"), IdentifierKind::Other);
    }
}
