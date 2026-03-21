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

use tracing::warn;

use athen_core::error::Result as AthenResult;
use athen_core::event::*;
use athen_core::llm::{ChatMessage, MessageContent, Role};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskId, TaskPriority, TaskStatus, TaskStep};
use athen_core::traits::agent::{AgentExecutor, StepAuditor};
use athen_core::traits::llm::LlmRouter;
use athen_agent::{AgentBuilder, InMemoryAuditor, ShellToolRegistry};
use athen_persistence::chat::SessionMeta;

use crate::state::{AppState, PendingApproval, SharedRouter};

/// A simplified chat message suitable for returning to the frontend.
#[derive(Serialize)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Persist a chat message to SQLite (fire-and-forget; errors are logged, not propagated).
///
/// Also updates the session's `updated_at` timestamp.
async fn persist_message(state: &AppState, role: &str, content: &str) {
    if let Some(ref store) = state.chat_store {
        let session_id = state.session_id.lock().await.clone();
        if let Err(e) = store.save_message(&session_id, role, content, "text").await {
            warn!("Failed to persist chat message: {e}");
        }
        if let Err(e) = store.touch_session(&session_id).await {
            warn!("Failed to touch session: {e}");
        }
    }
}

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
    /// Tool arguments or result summary (truncated to ~200 chars).
    detail: Option<String>,
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

    /// Truncate a detail string to `max_len` characters, appending "..." if truncated.
    /// Replaces newlines with spaces for compact display.
    fn truncate_detail(s: &str, max_len: usize) -> String {
        let compacted = s.replace('\n', " ");
        let trimmed = compacted.trim();
        if trimmed.len() <= max_len {
            trimmed.to_string()
        } else {
            format!("{}...", &trimmed[..max_len])
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

        // Extract a useful detail string from the step output.
        let detail = step.output.as_ref().and_then(|output| {
            // For tool calls, show the command/path/key from the arguments or result.
            if let Some(tool) = output.get("tool").and_then(|t| t.as_str()) {
                // Try to build a summary from the tool result.
                if let Some(result) = output.get("result") {
                    let summary = match tool {
                        "shell_execute" => result
                            .get("stdout")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string()),
                        "read_file" | "write_file" => result
                            .get("path")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string()),
                        "list_directory" => result
                            .get("path")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string()),
                        _ => Some(
                            serde_json::to_string(result)
                                .unwrap_or_default(),
                        ),
                    };
                    return summary.map(|s| Self::truncate_detail(&s, 200));
                }
                // If there was an error, show it.
                if let Some(err) = output.get("error").and_then(|e| e.as_str()) {
                    return Some(Self::truncate_detail(err, 200));
                }
            }
            // For completion steps, show a brief response preview.
            if let Some(response) = output.get("response").and_then(|r| r.as_str()) {
                return Some(Self::truncate_detail(response, 200));
            }
            None
        });

        let _ = self.app_handle.emit(
            "agent-progress",
            AgentProgress {
                step: step.index + 1,
                tool_name,
                status: format!("{:?}", step.status),
                detail,
            },
        );
        self.inner.record_step(task_id, step).await
    }

    async fn get_steps(&self, task_id: TaskId) -> AthenResult<Vec<TaskStep>> {
        self.inner.get_steps(task_id).await
    }
}

/// Spawn a background task that forwards streaming text deltas from the
/// executor to the frontend via Tauri events.
///
/// Returns the sender half of the channel that should be passed to
/// `AgentBuilder::stream_sender()`.
fn spawn_stream_forwarder(
    app_handle: &AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let handle = app_handle.clone();
    tokio::spawn(async move {
        while let Some(delta) = rx.recv().await {
            let _ = handle.emit(
                "agent-stream",
                serde_json::json!({ "delta": delta, "is_final": false }),
            );
        }
        // Channel closed -- emit a final marker so the frontend knows
        // the stream is complete.
        let _ = handle.emit(
            "agent-stream",
            serde_json::json!({ "delta": "", "is_final": true }),
        );
    });
    tx
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
            detail: None,
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

            let auditor = TauriAuditor::new(app_handle.clone());

            // Set up streaming: forward LLM text chunks to the frontend
            // in real time via Tauri events.
            let stream_tx = spawn_stream_forwarder(&app_handle);

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(25)
                .timeout(Duration::from_secs(90))
                .context_messages(context)
                .stream_sender(stream_tx)
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
                        content: MessageContent::Text(message.clone()),
                    });
                    history.push(ChatMessage {
                        role: Role::Assistant,
                        content: MessageContent::Text(msg.clone()),
                    });
                    drop(history);
                    persist_message(&state, "user", &message).await;
                    persist_message(&state, "assistant", &msg).await;
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
                    content: MessageContent::Text(message.clone()),
                });
                history.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(content.clone()),
                });
            }
            persist_message(&state, "user", &message).await;
            persist_message(&state, "assistant", &content).await;

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
            let auditor = TauriAuditor::new(app_handle.clone());

            // Set up streaming for the approved task execution.
            let stream_tx = spawn_stream_forwarder(&app_handle);

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(25)
                .timeout(Duration::from_secs(90))
                .context_messages(context)
                .stream_sender(stream_tx)
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
                        content: MessageContent::Text(message.clone()),
                    });
                    history.push(ChatMessage {
                        role: Role::Assistant,
                        content: MessageContent::Text(msg.clone()),
                    });
                    drop(history);
                    persist_message(&state, "user", &message).await;
                    persist_message(&state, "assistant", &msg).await;
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
                    content: MessageContent::Text(message.clone()),
                });
                history.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(content.clone()),
                });
            }
            persist_message(&state, "user", &message).await;
            persist_message(&state, "assistant", &content).await;

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
        model: state.model_name.lock().await.clone(),
    })
}

/// Start a fresh conversation session.
///
/// Clears the in-memory history and generates a new session identifier.
/// Previous sessions remain in SQLite and can be loaded later.
/// Returns the new session ID so the frontend can update the sidebar.
#[tauri::command]
pub async fn new_session(
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let new_id = chrono::Utc::now()
        .format("session_%Y%m%d_%H%M%S")
        .to_string();

    // Create session metadata entry.
    if let Some(ref store) = state.chat_store {
        if let Err(e) = store.create_session(&new_id, "New Chat").await {
            warn!("Failed to create session metadata: {e}");
        }
    }

    let mut history = state.history.lock().await;
    history.clear();
    drop(history);

    let mut session_id = state.session_id.lock().await;
    *session_id = new_id.clone();

    Ok(new_id)
}

/// Return the current session's conversation history for the frontend
/// to render on startup.
///
/// Only User and Assistant messages are returned (tool messages are
/// filtered out since they are not meaningful for display).
#[tauri::command]
pub async fn get_history(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<HistoryMessage>, String> {
    let history = state.history.lock().await;
    let messages: Vec<HistoryMessage> = history
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "tool",
            };
            let content = match &m.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Structured(v) => {
                    serde_json::to_string_pretty(v).unwrap_or_default()
                }
            };
            HistoryMessage {
                role: role.to_string(),
                content,
            }
        })
        .collect();
    Ok(messages)
}

/// List all sessions with metadata for the sidebar.
#[tauri::command]
pub async fn list_sessions(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<SessionMeta>, String> {
    let store = state
        .chat_store
        .as_ref()
        .ok_or_else(|| "Chat store not available".to_string())?;
    store
        .list_sessions_with_meta()
        .await
        .map_err(|e| e.to_string())
}

/// Switch to a different session, loading its messages into the in-memory history.
///
/// Returns the loaded messages so the frontend can render them immediately.
#[tauri::command]
pub async fn switch_session(
    session_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<HistoryMessage>, String> {
    let store = state
        .chat_store
        .as_ref()
        .ok_or_else(|| "Chat store not available".to_string())?;

    // Load the persisted messages for the target session.
    let persisted = store
        .load_messages(&session_id)
        .await
        .map_err(|e| e.to_string())?;

    let chat_messages: Vec<ChatMessage> = persisted
        .iter()
        .map(|m| ChatMessage {
            role: match m.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                "tool" => Role::Tool,
                _ => Role::User,
            },
            content: if m.content_type == "structured" {
                match serde_json::from_str(&m.content) {
                    Ok(v) => MessageContent::Structured(v),
                    Err(_) => MessageContent::Text(m.content.clone()),
                }
            } else {
                MessageContent::Text(m.content.clone())
            },
        })
        .collect();

    // Build the display messages (user + assistant only).
    let display: Vec<HistoryMessage> = persisted
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| HistoryMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    // Swap in-memory state.
    let mut history = state.history.lock().await;
    *history = chat_messages;
    drop(history);

    let mut current_session = state.session_id.lock().await;
    *current_session = session_id;

    Ok(display)
}

/// Rename a session.
#[tauri::command]
pub async fn rename_session(
    session_id: String,
    name: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let store = state
        .chat_store
        .as_ref()
        .ok_or_else(|| "Chat store not available".to_string())?;
    store
        .rename_session(&session_id, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Delete a session and all its messages.
///
/// Returns the session ID of the session that should become active
/// (the most recent remaining session, or a newly created one).
#[tauri::command]
pub async fn delete_session(
    session_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let store = state
        .chat_store
        .as_ref()
        .ok_or_else(|| "Chat store not available".to_string())?;

    store
        .delete_session(&session_id)
        .await
        .map_err(|e| e.to_string())?;

    let current = state.session_id.lock().await.clone();

    // If we deleted the active session, switch to another.
    if current == session_id {
        let sessions = store
            .list_sessions_with_meta()
            .await
            .map_err(|e| e.to_string())?;

        if let Some(next) = sessions.first() {
            // Load the next session's history.
            let persisted = store
                .load_messages(&next.session_id)
                .await
                .map_err(|e| e.to_string())?;

            let chat_messages: Vec<ChatMessage> = persisted
                .iter()
                .map(|m| ChatMessage {
                    role: match m.role.as_str() {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "system" => Role::System,
                        "tool" => Role::Tool,
                        _ => Role::User,
                    },
                    content: if m.content_type == "structured" {
                        match serde_json::from_str(&m.content) {
                            Ok(v) => MessageContent::Structured(v),
                            Err(_) => MessageContent::Text(m.content.clone()),
                        }
                    } else {
                        MessageContent::Text(m.content.clone())
                    },
                })
                .collect();

            let mut history = state.history.lock().await;
            *history = chat_messages;
            drop(history);

            let mut sid = state.session_id.lock().await;
            *sid = next.session_id.clone();

            return Ok(next.session_id.clone());
        }

        // No sessions left -- create a new one.
        let new_id = chrono::Utc::now()
            .format("session_%Y%m%d_%H%M%S")
            .to_string();
        if let Err(e) = store.create_session(&new_id, "New Chat").await {
            warn!("Failed to create replacement session: {e}");
        }
        let mut history = state.history.lock().await;
        history.clear();
        drop(history);
        let mut sid = state.session_id.lock().await;
        *sid = new_id.clone();
        return Ok(new_id);
    }

    Ok(current)
}

/// Return the current active session ID.
#[tauri::command]
pub async fn get_current_session(
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    Ok(state.session_id.lock().await.clone())
}
