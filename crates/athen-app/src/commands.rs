//! Tauri IPC command handlers.
//!
//! Each `#[tauri::command]` function is callable from the frontend
//! via `window.__TAURI__.core.invoke(...)`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use athen_core::event::*;
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::llm::LlmRouter;
use athen_agent::{AgentBuilder, ShellToolRegistry};

use crate::state::AppState;
use crate::state::SharedRouter;

/// Response payload returned to the frontend after processing a chat message.
#[derive(Serialize)]
pub struct ChatResponse {
    pub content: String,
    pub risk_level: Option<String>,
    pub domain: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,
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
        });
    }

    // Try to dispatch the next pending task to an available agent.
    match state.coordinator.dispatch_next().await {
        Ok(Some((task_id, _))) => {
            // Build executor with real tool execution (same as athen-cli).
            let exec_router: Box<dyn LlmRouter> =
                Box::new(SharedRouter(Arc::clone(&state.router)));
            let registry = ShellToolRegistry::new().await;

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .max_steps(20)
                .timeout(Duration::from_secs(120))
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

            let result = executor.execute(task).await.map_err(|e| e.to_string())?;

            // Extract response content from the executor output.
            let content = result
                .output
                .as_ref()
                .and_then(|o| o.get("response"))
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();

            // If no response text but we have output, show the full output.
            let content = if content.is_empty() {
                result
                    .output
                    .as_ref()
                    .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                    .unwrap_or_else(|| "Task completed.".to_string())
            } else {
                content
            };

            // Mark coordinator task as completed.
            let _ = state.coordinator.complete_task(task_id).await;

            Ok(ChatResponse {
                content,
                risk_level: Some(
                    if result.success { "Safe" } else { "Caution" }.into(),
                ),
                domain: Some("base".into()),
                tool_calls: vec![],
            })
        }
        Ok(None) => Ok(ChatResponse {
            content: "Action blocked by risk system or no agent available.".into(),
            risk_level: Some("Danger".into()),
            domain: None,
            tool_calls: vec![],
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// Return basic status information.
#[tauri::command]
pub async fn get_status() -> std::result::Result<StatusResponse, String> {
    Ok(StatusResponse {
        connected: true,
        model: "deepseek-chat".into(),
    })
}
