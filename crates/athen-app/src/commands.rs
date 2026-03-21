//! Tauri IPC command handlers.
//!
//! Each `#[tauri::command]` function is callable from the frontend
//! via `window.__TAURI__.core.invoke(...)`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use athen_core::error::Result as AthenResult;
use athen_core::event::*;
use athen_core::llm::{ChatMessage, MessageContent, Role};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskId, TaskPriority, TaskStatus, TaskStep};
use athen_core::traits::agent::{AgentExecutor, StepAuditor};
use athen_core::traits::llm::LlmRouter;
use athen_agent::{AgentBuilder, InMemoryAuditor, ShellToolRegistry};

use crate::state::{AppState, PendingApproval, SharedRouter};

/// Response payload returned to the frontend after processing a chat message.
#[derive(Serialize)]
pub struct ChatResponse {
    pub content: String,
    pub risk_level: Option<String>,
    pub domain: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,
    /// Present when the action requires human approval before execution.
    pub pending_approval: Option<PendingApproval>,
}

/// Summary of a tool call for the frontend to display.
#[derive(Serialize)]
pub struct ToolCallInfo {
    pub name: String,
    pub summary: String,
}

/// Simple status payload for health/connectivity checks.
#[derive(Serialize)]
pub struct StatusResponse {
    pub connected: bool,
    pub model: String,
}

/// Progress event emitted to the frontend during agent execution.
#[derive(Clone, Serialize)]
struct AgentProgress {
    step: u32,
    tool_name: String,
    status: String,
}

/// Step auditor that emits Tauri events for real-time progress in the UI.
struct TauriAuditor {
    inner: InMemoryAuditor,
    app_handle: AppHandle,
}

impl TauriAuditor {
    fn new(app_handle: AppHandle) -> Self {
        Self {
            inner: InMemoryAuditor::new(),
            app_handle,
        }
    }
}

#[async_trait]
impl StepAuditor for TauriAuditor {
    async fn record_step(&self, task_id: TaskId, step: &TaskStep) -> AthenResult<()> {
        // Emit progress event to the frontend.
        let tool_name = step
            .description
            .strip_prefix("Tool call: ")
            .unwrap_or(&step.description)
            .to_string();
        let _ = self.app_handle.emit(
            "agent-progress",
            AgentProgress {
                step: step.index + 1,
                tool_name,
                status: format!("{:?}", step.status),
            },
        );
        self.inner.record_step(task_id, step).await
    }

    async fn get_steps(&self, task_id: TaskId) -> AthenResult<Vec<TaskStep>> {
        self.inner.get_steps(task_id).await
    }
}

/// Process a user message through the coordinator and agent executor.
///
/// 1. Creates a `SenseEvent` from the user input.
/// 2. Routes it through the coordinator (risk evaluation + task creation).
/// 3. Dispatches to an agent slot.
/// 4. Builds a full `AgentExecutor` with `ShellToolRegistry` for real tool execution.
/// 5. Returns the agent's response with risk and domain metadata.
#[tauri::command]
pub async fn send_message(
    message: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> std::result::Result<ChatResponse, String> {
    // Build a SenseEvent from the user's text input.
    let event = SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::UserInput,
        kind: EventKind::Command,
        sender: None,
        content: NormalizedContent {
            summary: Some(message.clone()),
            body: serde_json::json!(message),
            attachments: vec![],
        },
        source_risk: RiskLevel::Safe,
        raw_id: None,
    };

    // Emit status for risk evaluation phase.
    let _ = app_handle.emit(
        "agent-progress",
        AgentProgress {
            step: 0,
            tool_name: "Evaluating risk...".to_string(),
            status: "InProgress".to_string(),
        },
    );

    // Route the event through the coordinator (risk + queue).
    let task_ids = state
        .coordinator
        .process_event(event)
        .await
        .map_err(|e| e.to_string())?;

    if task_ids.is_empty() {
        return Ok(ChatResponse {
            content: "No tasks created.".into(),
            risk_level: None,
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        });
    }

    // Check if the task was flagged for human approval.
    if let Some(awaiting_task) = state.coordinator.get_awaiting_approval().await {
        let risk_score = awaiting_task
            .risk_score
            .as_ref()
            .map(|s| s.total)
            .unwrap_or(0.0);
        let risk_level = awaiting_task
            .risk_score
            .as_ref()
            .map(|s| format!("{:?}", s.level))
            .unwrap_or_else(|| "Unknown".into());

        // Stash the original message so we can replay it after approval.
        *state.pending_message.lock().await = Some(message.clone());

        let approval = PendingApproval {
            task_id: awaiting_task.id.to_string(),
            description: awaiting_task.description.clone(),
            risk_score,
            risk_level: risk_level.clone(),
        };

        return Ok(ChatResponse {
            content: format!(
                "This action requires your approval before it can be executed.\n\
                 Risk score: {:.0} ({risk_level})",
                risk_score
            ),
            risk_level: Some(risk_level),
            domain: Some(format!("{:?}", awaiting_task.domain)),
            tool_calls: vec![],
            pending_approval: Some(approval),
        });
    }

    // Try to dispatch the next pending task to an available agent.
    match state.coordinator.dispatch_next().await {
        Ok(Some((task_id, _))) => {
            // Snapshot the current conversation history for context.
            let context = state.history.lock().await.clone();

            // Build executor with real tool execution (same as athen-cli).
            let exec_router: Box<dyn LlmRouter> =
                Box::new(SharedRouter(Arc::clone(&state.router)));
            let registry = ShellToolRegistry::new().await;

            let auditor = TauriAuditor::new(app_handle);

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(25)
                .timeout(Duration::from_secs(90))
                .context_messages(context)
                .build()
                .map_err(|e| e.to_string())?;

            // Create a task for the executor with the user's message.
            let task = Task {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                source_event: None,
                domain: DomainType::Base,
                description: message.clone(),
                priority: TaskPriority::Normal,
                status: TaskStatus::InProgress,
                risk_score: None,
                risk_budget: None,
                risk_used: 0,
                assigned_agent: None,
                steps: vec![],
                deadline: None,
            };

            let result = match executor.execute(task).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = state.coordinator.complete_task(task_id).await;
                    let msg = format!("Agent timed out: {e}");
                    let mut history = state.history.lock().await;
                    history.push(ChatMessage {
                        role: Role::User,
                        content: MessageContent::Text(message),
                    });
                    history.push(ChatMessage {
                        role: Role::Assistant,
                        content: MessageContent::Text(msg.clone()),
                    });
                    return Ok(ChatResponse {
                        content: msg,
                        risk_level: Some("Caution".into()),
                        domain: Some("base".into()),
                        tool_calls: vec![],
                        pending_approval: None,
                    });
                }
            };

            // Extract response content from the executor output.
            let content = if !result.success {
                // Handle max_steps_exceeded or other failures gracefully.
                let _reason = result
                    .output
                    .as_ref()
                    .and_then(|o| o.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                format!(
                    "I ran out of steps ({} used) before finishing. Try a simpler request or break it into smaller tasks.",
                    result.steps_completed
                )
            } else {
                let text = result
                    .output
                    .as_ref()
                    .and_then(|o| o.get("response"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    result
                        .output
                        .as_ref()
                        .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                        .unwrap_or_else(|| "Task completed.".to_string())
                } else {
                    text
                }
            };

            // Record the user message and assistant response in session history.
            {
                let mut history = state.history.lock().await;
                history.push(ChatMessage {
                    role: Role::User,
                    content: MessageContent::Text(message),
                });
                history.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(content.clone()),
                });
            }

            // Mark coordinator task as completed.
            let _ = state.coordinator.complete_task(task_id).await;

            Ok(ChatResponse {
                content,
                risk_level: Some(
                    if result.success { "Safe" } else { "Caution" }.into(),
                ),
                domain: Some("base".into()),
                tool_calls: vec![],
                pending_approval: None,
            })
        }
        Ok(None) => Ok(ChatResponse {
            content: "No agent available to handle this task. Please try again.".into(),
            risk_level: Some("Caution".into()),
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// Approve or deny a task that was flagged by the risk system.
///
/// When approved, the task is enqueued and dispatched to an agent for execution.
/// When denied, the task is cancelled and removed.
#[tauri::command]
pub async fn approve_task(
    task_id: String,
    approved: bool,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> std::result::Result<ChatResponse, String> {
    let task_uuid: Uuid = task_id.parse().map_err(|e| format!("Invalid task ID: {e}"))?;

    if !approved {
        // Deny the task.
        state
            .coordinator
            .deny_task(task_uuid)
            .await
            .map_err(|e| e.to_string())?;

        // Clear the stashed message.
        *state.pending_message.lock().await = None;

        return Ok(ChatResponse {
            content: "Action denied. The task has been cancelled.".into(),
            risk_level: Some("Safe".into()),
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        });
    }

    // Approve the task: move it to Pending and enqueue.
    let approved_task = state
        .coordinator
        .approve_task(task_uuid)
        .await
        .map_err(|e| e.to_string())?;

    // Retrieve the stashed user message for execution context.
    let message = state
        .pending_message
        .lock()
        .await
        .take()
        .unwrap_or_else(|| approved_task.description.clone());

    // Dispatch the now-enqueued task.
    match state.coordinator.dispatch_next().await {
        Ok(Some((coord_task_id, _))) => {
            let context = state.history.lock().await.clone();

            let exec_router: Box<dyn LlmRouter> =
                Box::new(SharedRouter(Arc::clone(&state.router)));
            let registry = ShellToolRegistry::new().await;
            let auditor = TauriAuditor::new(app_handle);

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(25)
                .timeout(Duration::from_secs(90))
                .context_messages(context)
                .build()
                .map_err(|e| e.to_string())?;

            let task = Task {
                id: Uuid::new_v4(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                source_event: None,
                domain: approved_task.domain.clone(),
                description: message.clone(),
                priority: approved_task.priority,
                status: TaskStatus::InProgress,
                risk_score: approved_task.risk_score.clone(),
                risk_budget: approved_task.risk_budget,
                risk_used: approved_task.risk_used,
                assigned_agent: None,
                steps: vec![],
                deadline: None,
            };

            let result = match executor.execute(task).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = state.coordinator.complete_task(coord_task_id).await;
                    let msg = format!("Agent error after approval: {e}");
                    let mut history = state.history.lock().await;
                    history.push(ChatMessage {
                        role: Role::User,
                        content: MessageContent::Text(message),
                    });
                    history.push(ChatMessage {
                        role: Role::Assistant,
                        content: MessageContent::Text(msg.clone()),
                    });
                    return Ok(ChatResponse {
                        content: msg,
                        risk_level: Some("Caution".into()),
                        domain: Some(format!("{:?}", approved_task.domain)),
                        tool_calls: vec![],
                        pending_approval: None,
                    });
                }
            };

            let content = if !result.success {
                format!(
                    "I ran out of steps ({} used) before finishing. Try a simpler request.",
                    result.steps_completed
                )
            } else {
                let text = result
                    .output
                    .as_ref()
                    .and_then(|o| o.get("response"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    result
                        .output
                        .as_ref()
                        .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                        .unwrap_or_else(|| "Task completed.".to_string())
                } else {
                    text
                }
            };

            {
                let mut history = state.history.lock().await;
                history.push(ChatMessage {
                    role: Role::User,
                    content: MessageContent::Text(message),
                });
                history.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(content.clone()),
                });
            }

            let _ = state.coordinator.complete_task(coord_task_id).await;

            Ok(ChatResponse {
                content,
                risk_level: Some(
                    if result.success { "Safe" } else { "Caution" }.into(),
                ),
                domain: Some(format!("{:?}", approved_task.domain)),
                tool_calls: vec![],
                pending_approval: None,
            })
        }
        Ok(None) => Ok(ChatResponse {
            content: "Task approved but no agent is available. Please try again.".into(),
            risk_level: Some("Caution".into()),
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// Return basic status information.
#[tauri::command]
pub async fn get_status(
    state: State<'_, AppState>,
) -> std::result::Result<StatusResponse, String> {
    Ok(StatusResponse {
        connected: true,
        model: state.model_name.clone(),
    })
}
