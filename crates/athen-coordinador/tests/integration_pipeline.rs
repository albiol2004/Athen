//! Integration tests for the full event processing pipeline.
//!
//! These tests wire together real implementations from multiple crates
//! (athen-sentidos, athen-coordinador, athen-risk) with only the LLM
//! router mocked.

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::error::Result;
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::llm::{BudgetStatus, FinishReason, LlmRequest, LlmResponse, TokenUsage};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, TaskPriority, TaskStatus};
use athen_core::traits::coordinator::TaskQueue;
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::sense::SenseMonitor;

use athen_coordinador::Coordinator;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;
use athen_sentidos::user_input::UserInputMonitor;

// ---------------------------------------------------------------------------
// Mock LLM router -- the ONLY mock in these integration tests.
// Returns a low-risk JSON response so the LLM fallback path produces a
// safe score for benign inputs.
// ---------------------------------------------------------------------------

struct MockLlmRouter;

#[async_trait]
impl LlmRouter for MockLlmRouter {
    async fn route(&self, _request: &LlmRequest) -> Result<LlmResponse> {
        Ok(LlmResponse {
            content: r#"{"impact":"read","sensitivity":"plain","confidence":0.95}"#.to_string(),
            model_used: "mock-model".to_string(),
            provider: "mock-provider".to_string(),
            usage: TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                estimated_cost_usd: None,
            },
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
        })
    }

    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        Ok(BudgetStatus {
            daily_limit_usd: None,
            spent_today_usd: 0.0,
            remaining_usd: None,
            tokens_used_today: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Helper: build a Coordinator backed by real risk evaluation.
// ---------------------------------------------------------------------------

fn make_coordinator() -> Coordinator {
    let llm_evaluator = LlmRiskEvaluator::new(Box::new(MockLlmRouter));
    let combined = CombinedRiskEvaluator::new(llm_evaluator);
    Coordinator::new(Box::new(combined))
}

// ---------------------------------------------------------------------------
// Helper: build a SenseEvent by hand (for tests that do not go through the
// UserInputMonitor).
// ---------------------------------------------------------------------------

fn make_event(source: EventSource, kind: EventKind, summary: &str) -> SenseEvent {
    SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source,
        kind,
        sender: None,
        content: NormalizedContent {
            summary: Some(summary.to_string()),
            body: serde_json::json!(summary),
            attachments: Vec::new(),
        },
        source_risk: RiskLevel::Safe,
        raw_id: None,
    }
}

// ===========================================================================
// Test 1: User input flows through UserInputMonitor -> Coordinator
// ===========================================================================

#[tokio::test]
async fn test_user_input_flows_to_coordinator() {
    // 1. Create a UserInputMonitor and grab its sender handle.
    let monitor = UserInputMonitor::new(16);
    let tx = monitor.sender();

    // 2. Create a Coordinator with real router, queue, dispatcher, and risk evaluator.
    let coordinator = make_coordinator();

    // 3. Send a benign message through the user input sender.
    tx.send("Hello, what's the weather?".to_string())
        .await
        .unwrap();

    // 4. Poll the monitor to get a SenseEvent.
    let events = monitor.poll().await.unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event from poll");

    let event = events.into_iter().next().unwrap();
    assert_eq!(event.source, EventSource::UserInput);

    // 5. Process the event through the coordinator.
    let results = coordinator.process_event(event).await.unwrap();
    assert_eq!(results.len(), 1, "expected exactly one task created");

    // 6. The task should be enqueued as Pending (benign input, low risk).
    //    Dequeue it to inspect its fields.
    let task = coordinator
        .queue()
        .dequeue()
        .await
        .unwrap()
        .expect("task should be in the queue");

    assert_eq!(task.domain, DomainType::Base);
    assert_eq!(task.priority, TaskPriority::High);
    assert_eq!(task.status, TaskStatus::Pending);
}

// ===========================================================================
// Test 2: Dangerous command gets blocked or requires approval
// ===========================================================================

#[tokio::test]
async fn test_dangerous_command_gets_blocked() {
    let coordinator = make_coordinator();

    // Build an event that looks like it came from an external source with
    // dangerous content. The RuleEngine detects "sudo rm -rf /".
    // The Coordinator uses TrustLevel::Neutral (2.0x) by default.
    //
    // sudo -> System(90), rm -rf -> also System(90). The rule engine picks
    // the highest impact. With Neutral trust (2.0x) and Plain data (1.0x):
    //   total = 90 * 2.0 * 1.0 + 0 = 180 -> HardBlock (>= 90)
    let event = make_event(
        EventSource::System,
        EventKind::Command,
        "sudo rm -rf /",
    );

    let results = coordinator.process_event(event).await.unwrap();
    assert_eq!(results.len(), 1);

    // The task should NOT be in the queue (it was blocked or held for approval).
    let pending = coordinator.queue().pending_count().await.unwrap();
    assert_eq!(pending, 0, "dangerous task must not be enqueued");

    // We cannot directly inspect the task status through the Coordinator's
    // public API after processing (tasks that are not Pending are not
    // enqueued), but we verified it was not enqueued. The risk score
    // for "sudo rm -rf /" with Neutral trust will be >= 90 (HardBlock)
    // or >= 50 (HumanConfirm), either way the task is not dispatched.
}

// ===========================================================================
// Test 3: Dispatch assigns task to agent, complete releases agent
// ===========================================================================

#[tokio::test]
async fn test_dispatch_assigns_to_agent() {
    let coordinator = make_coordinator();

    // 1. Register an agent.
    let agent_id = Uuid::new_v4();
    coordinator.dispatcher().register_agent(agent_id).await;

    // 2. Process a benign user input event.
    let event = make_event(
        EventSource::UserInput,
        EventKind::Command,
        "list my files",
    );
    coordinator.process_event(event).await.unwrap();

    // 3. Dispatch the next task.
    let result = coordinator.dispatch_next().await.unwrap();
    assert!(result.is_some(), "dispatch should succeed with an available agent");

    let (task_id, dispatched_agent) = result.unwrap();

    // 4. Assert the task was assigned to our registered agent.
    assert_eq!(dispatched_agent, agent_id);

    // Verify through the dispatcher that the agent is assigned.
    let assigned = coordinator.dispatcher().assigned_agent(task_id).await;
    assert_eq!(assigned, Some(agent_id));

    // 5. Complete the task.
    coordinator.complete_task(task_id).await.unwrap();

    // 6. Agent should be available again (no longer assigned).
    let assigned_after = coordinator.dispatcher().assigned_agent(task_id).await;
    assert_eq!(assigned_after, None, "agent should be released after task completion");

    // Verify the agent can be assigned a new task.
    let event2 = make_event(
        EventSource::UserInput,
        EventKind::Command,
        "check my email",
    );
    coordinator.process_event(event2).await.unwrap();

    let result2 = coordinator.dispatch_next().await.unwrap();
    assert!(result2.is_some(), "agent should be available for a new task");
    let (_, agent2) = result2.unwrap();
    assert_eq!(agent2, agent_id);
}

// ===========================================================================
// Test 4: Priority ordering -- High before Normal
// ===========================================================================

#[tokio::test]
async fn test_priority_ordering() {
    let coordinator = make_coordinator();

    // Register an agent so dispatch works.
    let agent_id = Uuid::new_v4();
    coordinator.dispatcher().register_agent(agent_id).await;

    // Process an email event first (Normal priority).
    let email_event = make_event(
        EventSource::Email,
        EventKind::NewMessage,
        "Meeting notes from today",
    );
    let email_results = coordinator.process_event(email_event).await.unwrap();
    assert_eq!(email_results.len(), 1);

    // Process a user input event second (High priority).
    let user_event = make_event(
        EventSource::UserInput,
        EventKind::Command,
        "summarize my day",
    );
    let user_results = coordinator.process_event(user_event).await.unwrap();
    assert_eq!(user_results.len(), 1);

    // Both tasks should be in the queue.
    assert_eq!(coordinator.queue().pending_count().await.unwrap(), 2);

    // First dispatch should return the High priority task (UserInput).
    let (first_task_id, _) = coordinator.dispatch_next().await.unwrap().unwrap();
    assert_eq!(
        first_task_id, user_results[0].0,
        "High priority user input task should be dispatched first"
    );

    // Complete the first task so the agent is available again.
    coordinator.complete_task(first_task_id).await.unwrap();

    // Second dispatch should return the Normal priority task (Email).
    let (second_task_id, _) = coordinator.dispatch_next().await.unwrap().unwrap();
    assert_eq!(
        second_task_id, email_results[0].0,
        "Normal priority email task should be dispatched second"
    );

    // Queue should now be empty.
    assert_eq!(coordinator.queue().pending_count().await.unwrap(), 0);
}
