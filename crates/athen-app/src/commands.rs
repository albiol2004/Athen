//! Tauri IPC command handlers.
//!
//! Each `#[tauri::command]` function is callable from the frontend
//! via `window.__TAURI__.core.invoke(...)`.

use std::sync::atomic::Ordering;
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
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_core::risk::{RiskDecision, RiskLevel};
use athen_core::task::{DomainType, Task, TaskId, TaskPriority, TaskStatus, TaskStep};
use athen_core::traits::agent::{AgentExecutor, StepAuditor};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::memory::MemoryStore;
use athen_agent::{AgentBuilder, InMemoryAuditor};
use athen_persistence::arcs;
use athen_persistence::calendar::CalendarEvent;

use crate::file_gate::{GrantDecision, PendingGrantSummary};
use crate::notifier::NotificationInfo;
use crate::state::{AppState, PendingApproval, SharedRouter};

/// Convert a raw technical error string into a user-friendly message.
///
/// Technical details are intentionally stripped — they are already logged
/// via `tracing` and available in console output for debugging.
fn format_user_error(err: &str) -> String {
    if err.contains("Timeout") {
        "The request took too long. Try a simpler question or check your internet connection."
            .into()
    } else if err.contains("request failed") || err.contains("Connection") {
        "Could not connect to the AI provider. Check your internet connection and API key in Settings."
            .into()
    } else if err.contains("auth") || err.contains("401") || err.contains("Unauthorized") {
        "Authentication failed. Please check your API key in Settings.".into()
    } else if err.contains("rate_limit") || err.contains("429") {
        "Rate limit reached. Please wait a moment and try again.".into()
    } else if err.contains("max_steps") {
        "I ran out of steps before finishing. Try breaking the task into smaller parts.".into()
    } else if err.contains("budget") || err.contains("Budget") {
        "Budget limit reached. Check your spending limits in Settings.".into()
    } else if err.contains("RiskThresholdExceeded") {
        "This action was blocked because it exceeds the allowed risk level.".into()
    } else {
        format!("Something went wrong: {}", simplify_error(err))
    }
}

/// Strip Rust-specific formatting from error strings for the fallback case.
///
/// Removes enum variant wrappers like `LlmProvider { provider: ..., message: ... }`
/// and extracts just the meaningful message portion.
fn simplify_error(err: &str) -> String {
    // Try to extract the "message: ..." portion from LlmProvider errors.
    if let Some(idx) = err.find("message: ") {
        let msg = &err[idx + 9..];
        // Strip trailing brace/whitespace.
        return msg.trim_end_matches('}').trim().to_string();
    }
    // Return the raw string if no simplification applies.
    err.to_string()
}

/// Extract key terms from a user message for broader memory search.
///
/// Filters out common stop words (Spanish + English) and short words,
/// returning meaningful terms that might match stored memories.
fn extract_key_terms(message: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        // Spanish
        "el", "la", "los", "las", "un", "una", "unos", "unas", "de", "del", "al",
        "en", "con", "por", "para", "que", "es", "son", "fue", "ser", "estar",
        "haz", "hay", "tiene", "tengo", "como", "pero", "más", "muy", "sin",
        "sobre", "entre", "este", "esta", "ese", "esa", "aqui", "ahi", "aquí",
        "ahí", "donde", "cuando", "quien", "cual", "todo", "toda", "todos",
        "mi", "tu", "su", "nos", "les", "me", "te", "se", "lo", "le",
        "quiero", "puedes", "puede", "hacer", "dime", "dame", "escribe",
        "escribeme", "aqui", "chat", "algo",
        // English
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could",
        "should", "may", "might", "can", "shall", "to", "of", "in", "for",
        "on", "with", "at", "by", "from", "and", "or", "but", "not", "no",
        "my", "your", "his", "her", "its", "our", "their", "this", "that",
        "what", "which", "who", "how", "when", "where", "why", "all", "each",
        "me", "you", "him", "it", "us", "them", "some", "any",
    ];

    message
        .split(|c: char| !c.is_alphanumeric() && c != 'á' && c != 'é' && c != 'í' && c != 'ó' && c != 'ú' && c != 'ñ' && c != 'ü')
        .filter(|w| {
            let lower = w.to_lowercase();
            lower.len() > 2 && !STOP_WORDS.contains(&lower.as_str())
        })
        .map(|w| w.to_string())
        .collect()
}

/// Reinforce graph edges for memories that were actually used in the response.
/// Uses keyword overlap to detect usage -- zero LLM cost.
async fn reinforce_used_memories(
    memory: &athen_memory::Memory,
    context: &[ChatMessage],
    response: &str,
) {
    let response_terms: std::collections::HashSet<String> = extract_key_terms(response)
        .into_iter()
        .map(|t| t.to_lowercase())
        .collect();

    if response_terms.is_empty() {
        return;
    }

    // Find the injected memory system message.
    let memory_msg = context.iter().find(|m| {
        matches!(m.role, Role::System)
            && matches!(&m.content, MessageContent::Text(t) if t.starts_with("Relevant information"))
    });

    let Some(ChatMessage {
        content: MessageContent::Text(memory_text),
        ..
    }) = memory_msg
    else {
        return;
    };

    for line in memory_text.lines() {
        let line = line.strip_prefix("- ").unwrap_or(line);
        let memory_terms: std::collections::HashSet<String> = extract_key_terms(line)
            .into_iter()
            .map(|t| t.to_lowercase())
            .collect();

        // If 2+ key terms overlap, this memory was used.
        let overlap = response_terms.intersection(&memory_terms).count();
        if overlap >= 2 {
            for term in &memory_terms {
                if let Err(e) = memory.reinforce_by_name(term, 0.1).await {
                    tracing::debug!("reinforce failed for {term}: {e}");
                }
            }
        }
    }
}

/// Judge whether a conversation exchange is worth storing in persistent memory.
///
/// Returns `Some(summary)` with a distilled summary if worth remembering,
/// or `None` if the interaction is trivial (greetings, small talk, repeated info).
/// Uses a cheap LLM call with a 15-second timeout. On failure, returns `None`
/// (better to skip than to store garbage).
async fn judge_worth_remembering(
    router: &dyn LlmRouter,
    user_msg: &str,
    assistant_msg: &str,
) -> Option<String> {
    use athen_core::llm::{LlmRequest, ModelProfile, ChatMessage as LlmChatMessage, MessageContent as LlmContent, Role as LlmRole};

    let prompt = format!(
        "Analyze this conversation exchange and decide if it contains information worth remembering for future conversations.\n\n\
         User: {user_msg}\n\
         Assistant: {assistant_msg}\n\n\
         Worth remembering: facts about people, preferences, relationships, decisions, plans, \
         important events, personal details the user shared, or things the user explicitly asked to remember.\n\
         NOT worth remembering: greetings, small talk, questions about capabilities, \
         generic requests (write a poem, translate), or information the assistant already has from tools.\n\n\
         If worth remembering, respond with ONLY a concise summary of the key facts (1-2 sentences, no fluff).\n\
         If NOT worth remembering, respond with exactly: SKIP"
    );

    let request = LlmRequest {
        profile: ModelProfile::Cheap,
        messages: vec![LlmChatMessage {
            role: LlmRole::User,
            content: LlmContent::Text(prompt),
        }],
        max_tokens: Some(100),
        temperature: Some(0.0),
        tools: None,
        system_prompt: None,
    };

    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        router.route(&request),
    ).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::debug!("Memory judge LLM failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("Memory judge timed out");
            return None;
        }
    };

    let text = response.content.trim().to_string();

    // Check if the model said SKIP (or any variation).
    if text.is_empty()
        || text.eq_ignore_ascii_case("SKIP")
        || text.to_uppercase().starts_with("SKIP")
        || text.starts_with("NOT ")
        || text.starts_with("No ")
    {
        return None;
    }

    // Strip any "REMEMBER:" or "Summary:" prefix the model might add.
    let cleaned = text
        .strip_prefix("REMEMBER:")
        .or_else(|| text.strip_prefix("Summary:"))
        .unwrap_or(&text)
        .trim()
        .to_string();

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Persist an entry to the active Arc in SQLite (fire-and-forget; errors are logged, not propagated).
///
/// Also updates the arc's `updated_at` timestamp.
async fn persist_entry(
    state: &AppState,
    source: &str,
    content: &str,
    entry_type: &str,
    metadata: Option<serde_json::Value>,
) {
    if let Some(ref store) = state.arc_store {
        let arc_id = state.active_arc_id.lock().await.clone();
        let et = arcs::EntryType::from_str(entry_type);
        if let Err(e) = store.add_entry(&arc_id, et, source, content, metadata).await {
            warn!("Failed to persist arc entry: {e}");
        }
        if let Err(e) = store.touch_arc(&arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }
}

/// Response type for arc entries returned to the frontend.
#[derive(Serialize)]
pub struct ArcEntryResponse {
    pub id: i64,
    pub entry_type: String,
    pub source: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
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
pub(crate) struct AgentProgress {
    pub step: u32,
    pub tool_name: String,
    pub status: String,
    /// Tool arguments or result summary (truncated to ~200 chars).
    pub detail: Option<String>,
}

/// Step auditor that emits Tauri events for real-time progress in the UI.
pub(crate) struct TauriAuditor {
    inner: InMemoryAuditor,
    app_handle: AppHandle,
}

impl TauriAuditor {
    pub(crate) fn new(app_handle: AppHandle) -> Self {
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
        if trimmed.chars().count() <= max_len {
            trimmed.to_string()
        } else {
            let end: usize = trimmed
                .char_indices()
                .nth(max_len)
                .map(|(i, _)| i)
                .unwrap_or(trimmed.len());
            format!("{}...", &trimmed[..end])
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
                        "read" | "write" | "edit" => result
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
pub(crate) fn spawn_stream_forwarder(
    app_handle: &AppHandle,
    arc_id: Option<String>,
) -> tokio::sync::mpsc::UnboundedSender<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let handle = app_handle.clone();
    tokio::spawn(async move {
        while let Some(delta) = rx.recv().await {
            // Check for STX prefix (\x02) which marks thinking/reasoning content.
            let (actual_delta, is_thinking) = if delta.starts_with('\x02') {
                (delta['\x02'.len_utf8()..].to_string(), true)
            } else {
                (delta, false)
            };
            let _ = handle.emit(
                "agent-stream",
                serde_json::json!({ "delta": actual_delta, "is_final": false, "arc_id": arc_id, "is_thinking": is_thinking }),
            );
        }
        // Channel closed -- emit a final marker so the frontend knows
        // the stream is complete.
        let _ = handle.emit(
            "agent-stream",
            serde_json::json!({ "delta": "", "is_final": true, "arc_id": arc_id, "is_thinking": false }),
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
    let task_results = state
        .coordinator
        .process_event(event)
        .await
        .map_err(|e| {
            let raw = e.to_string();
            tracing::error!("Coordinator process_event failed: {raw}");
            format_user_error(&raw)
        })?;

    if task_results.is_empty() {
        return Ok(ChatResponse {
            content: "No tasks created.".into(),
            risk_level: None,
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        });
    }

    // Notify on NotifyAndProceed decisions (medium-risk auto-executed tasks).
    let current_arc_for_notif = state.active_arc_id.lock().await.clone();
    for (task_id, decision) in &task_results {
        if matches!(decision, RiskDecision::NotifyAndProceed) {
            if let Some(ref notifier) = state.notifier {
                let notification = Notification {
                    id: Uuid::new_v4(),
                    urgency: NotificationUrgency::Medium,
                    title: "Task auto-executed".to_string(),
                    body: "A medium-risk task was automatically executed.".to_string(),
                    origin: NotificationOrigin::RiskSystem,
                    arc_id: Some(current_arc_for_notif.clone()),
                    task_id: Some(*task_id),
                    created_at: chrono::Utc::now(),
                    requires_response: false,
                };
                notifier.notify(notification).await;
            }
        }
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
    // If no agent is available (stale assignment from a previous task),
    // force-release all and retry once.
    let dispatch_result = match state.coordinator.dispatch_next().await {
        Ok(None) => {
            tracing::warn!("No agent available, force-releasing stale assignments");
            state.coordinator.dispatcher().force_release_all().await;
            state.coordinator.dispatch_next().await
        }
        other => other,
    };
    match dispatch_result {
        Ok(Some((task_id, _))) => {
            // Snapshot the current conversation history for context.
            let mut context = state.history.lock().await.clone();

            // Auto-inject relevant memories into context.
            // Search with the full message AND with individual key terms
            // to catch indirect references (e.g., "mi novia" → finds "Nadia es mi novia").
            if let Some(ref memory) = state.memory {
                let mut all_items = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();

                // 1. Full message search.
                if let Ok(items) = memory.recall(&message, 5).await {
                    for item in items {
                        if seen_ids.insert(item.id.clone()) {
                            all_items.push(item);
                        }
                    }
                }

                // 2. Extract key terms and search each one for broader coverage.
                let key_terms = extract_key_terms(&message);
                for term in &key_terms {
                    if let Ok(items) = memory.recall(term, 3).await {
                        for item in items {
                            if seen_ids.insert(item.id.clone()) {
                                all_items.push(item);
                            }
                        }
                    }
                }

                // Limit to top 5 total.
                all_items.truncate(5);

                if !all_items.is_empty() {
                    tracing::info!(
                        count = all_items.len(),
                        "Injecting relevant memories into context"
                    );
                    let memory_text = all_items
                        .iter()
                        .map(|m| format!("- {}", m.content))
                        .collect::<Vec<_>>()
                        .join("\n");
                    context.insert(0, ChatMessage {
                        role: Role::System,
                        content: MessageContent::Text(format!(
                            "Relevant information from your memory:\n{memory_text}"
                        )),
                    });
                } else {
                    tracing::debug!("No relevant memories found for query");
                }
            }

            // Build executor with real tool execution (same as athen-cli).
            let exec_router: Box<dyn LlmRouter> =
                Box::new(SharedRouter(Arc::clone(&state.router)));
            let arc_for_registry = state.active_arc_id.lock().await.clone();
            let registry = state
                .build_tool_registry(&arc_for_registry, Some(app_handle.clone()))
                .await;

            let auditor = TauriAuditor::new(app_handle.clone());

            // Set up streaming: forward LLM text chunks to the frontend
            // in real time via Tauri events, tagged with the active arc.
            let current_arc = state.active_arc_id.lock().await.clone();
            let stream_tx = spawn_stream_forwarder(&app_handle, Some(current_arc));

            // Reset and wire the cancellation flag.
            let cancel_flag = Arc::clone(&state.cancel_flag);
            cancel_flag.store(false, Ordering::Relaxed);

            // Snapshot context for post-response reinforcement.
            let context_snapshot = context.clone();

            let mut builder = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(50)
                .timeout(Duration::from_secs(300))
                .context_messages(context)
                .stream_sender(stream_tx)
                .cancel_flag(cancel_flag);
            if let Some(p) = state.tool_doc_dir.clone() {
                builder = builder.tool_doc_dir(p);
            }
            let executor = builder
                .build()
                .map_err(|e| {
                    let raw = e.to_string();
                    tracing::error!("AgentBuilder failed: {raw}");
                    format_user_error(&raw)
                })?;

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
                    let raw = e.to_string();
                    tracing::error!("Agent execution failed: {raw}");
                    let msg = format_user_error(&raw);
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
                    persist_entry(&state, "user", &message, "message", None).await;
                    persist_entry(&state, "assistant", &msg, "message", None).await;
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
                let reason = result
                    .output
                    .as_ref()
                    .and_then(|o| o.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                if reason == "cancelled" {
                    "Task cancelled by user.".to_string()
                } else {
                    format!(
                        "I ran out of steps ({} used) before finishing. Try a simpler request or break it into smaller tasks.",
                        result.steps_completed
                    )
                }
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
            persist_entry(&state, "user", &message, "message", None).await;
            persist_entry(&state, "assistant", &content, "message", None).await;

            // Reinforce memories that were actually used in the response.
            if let Some(ref memory) = state.memory {
                reinforce_used_memories(memory, &context_snapshot, &content).await;
            }

            // Auto-remember: judge whether this interaction is worth storing,
            // then remember only a distilled summary (not greetings, small talk, etc.).
            if let Some(ref memory) = state.memory {
                let router = SharedRouter(Arc::clone(&state.router));
                let arc_id = state.active_arc_id.lock().await.clone();
                let msg_clone = message.clone();
                let content_clone = content.clone();
                let memory_clone = Arc::clone(memory);

                // Fire-and-forget in background so it doesn't block the response.
                tokio::spawn(async move {
                    match judge_worth_remembering(&router, &msg_clone, &content_clone).await {
                        Some(summary) => {
                            tracing::info!("Memory judge: worth remembering");
                            let item = athen_core::traits::memory::MemoryItem {
                                id: uuid::Uuid::new_v4().to_string(),
                                content: summary,
                                metadata: serde_json::json!({
                                    "source": "conversation",
                                    "arc_id": arc_id,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                }),
                            };
                            if let Err(e) = memory_clone.remember(item).await {
                                tracing::warn!("Failed to remember interaction: {e}");
                            }
                        }
                        None => {
                            tracing::debug!("Memory judge: not worth remembering, skipping");
                        }
                    }
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
        Err(e) => {
            let raw = e.to_string();
            tracing::error!("Dispatch failed: {raw}");
            Err(format_user_error(&raw))
        }
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
            .map_err(|e| {
                let raw = e.to_string();
                tracing::error!("Deny task failed: {raw}");
                format_user_error(&raw)
            })?;

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
        .map_err(|e| {
            let raw = e.to_string();
            tracing::error!("Approve task failed: {raw}");
            format_user_error(&raw)
        })?;

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
            let mut context = state.history.lock().await.clone();

            // Auto-inject relevant memories into context.
            if let Some(ref memory) = state.memory {
                let mut all_items = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();

                if let Ok(items) = memory.recall(&message, 5).await {
                    for item in items {
                        if seen_ids.insert(item.id.clone()) {
                            all_items.push(item);
                        }
                    }
                }
                let key_terms = extract_key_terms(&message);
                for term in &key_terms {
                    if let Ok(items) = memory.recall(term, 3).await {
                        for item in items {
                            if seen_ids.insert(item.id.clone()) {
                                all_items.push(item);
                            }
                        }
                    }
                }
                all_items.truncate(5);

                if !all_items.is_empty() {
                    tracing::info!(
                        count = all_items.len(),
                        "Injecting relevant memories into approved task context"
                    );
                    let memory_text = all_items
                        .iter()
                        .map(|m| format!("- {}", m.content))
                        .collect::<Vec<_>>()
                        .join("\n");
                    context.insert(0, ChatMessage {
                        role: Role::System,
                        content: MessageContent::Text(format!(
                            "Relevant information from your memory:\n{memory_text}"
                        )),
                    });
                }
            }

            let exec_router: Box<dyn LlmRouter> =
                Box::new(SharedRouter(Arc::clone(&state.router)));
            let arc_for_registry = state.active_arc_id.lock().await.clone();
            let registry = state
                .build_tool_registry(&arc_for_registry, Some(app_handle.clone()))
                .await;
            let auditor = TauriAuditor::new(app_handle.clone());

            // Set up streaming for the approved task execution.
            let current_arc = state.active_arc_id.lock().await.clone();
            let stream_tx = spawn_stream_forwarder(&app_handle, Some(current_arc));

            // Reset and wire the cancellation flag.
            let cancel_flag = Arc::clone(&state.cancel_flag);
            cancel_flag.store(false, Ordering::Relaxed);

            // Snapshot context for post-response reinforcement.
            let context_snapshot = context.clone();

            let mut builder = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .auditor(Box::new(auditor))
                .max_steps(50)
                .timeout(Duration::from_secs(300))
                .context_messages(context)
                .stream_sender(stream_tx)
                .cancel_flag(cancel_flag);
            if let Some(p) = state.tool_doc_dir.clone() {
                builder = builder.tool_doc_dir(p);
            }
            let executor = builder
                .build()
                .map_err(|e| {
                    let raw = e.to_string();
                    tracing::error!("AgentBuilder failed (approval): {raw}");
                    format_user_error(&raw)
                })?;

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
                    let raw = e.to_string();
                    tracing::error!("Agent execution failed after approval: {raw}");
                    let msg = format_user_error(&raw);
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
                    persist_entry(&state, "user", &message, "message", None).await;
                    persist_entry(&state, "assistant", &msg, "message", None).await;
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
                let reason = result
                    .output
                    .as_ref()
                    .and_then(|o| o.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                if reason == "cancelled" {
                    "Task cancelled by user.".to_string()
                } else {
                    format!(
                        "I ran out of steps ({} used) before finishing. Try a simpler request.",
                        result.steps_completed
                    )
                }
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
            persist_entry(&state, "user", &message, "message", None).await;
            persist_entry(&state, "assistant", &content, "message", None).await;

            // Reinforce memories that were actually used in the response.
            if let Some(ref memory) = state.memory {
                reinforce_used_memories(memory, &context_snapshot, &content).await;
            }

            // Auto-remember with LLM judge (same as send_message).
            if let Some(ref memory) = state.memory {
                let router = SharedRouter(Arc::clone(&state.router));
                let arc_id = state.active_arc_id.lock().await.clone();
                let msg_clone = message.clone();
                let content_clone = content.clone();
                let memory_clone = Arc::clone(memory);

                tokio::spawn(async move {
                    match judge_worth_remembering(&router, &msg_clone, &content_clone).await {
                        Some(summary) => {
                            tracing::info!("Memory judge: worth remembering (approved task)");
                            let item = athen_core::traits::memory::MemoryItem {
                                id: uuid::Uuid::new_v4().to_string(),
                                content: summary,
                                metadata: serde_json::json!({
                                    "source": "conversation",
                                    "arc_id": arc_id,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                }),
                            };
                            if let Err(e) = memory_clone.remember(item).await {
                                tracing::warn!("Failed to remember interaction: {e}");
                            }
                        }
                        None => {
                            tracing::debug!("Memory judge: not worth remembering (approved task)");
                        }
                    }
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
        Err(e) => {
            let raw = e.to_string();
            tracing::error!("Dispatch failed (approval): {raw}");
            Err(format_user_error(&raw))
        }
    }
}

/// Cancel the currently running agent task.
///
/// Sets the shared cancellation flag to `true`, which the executor checks
/// at the top of each loop iteration and between tool calls. The executor
/// will return a "cancelled" result on its next check.
#[tauri::command]
pub async fn cancel_task(
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    state.cancel_flag.store(true, Ordering::Relaxed);
    Ok(())
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

/// Start a fresh Arc.
///
/// Clears the in-memory history and generates a new Arc identifier.
/// Previous arcs remain in SQLite and can be loaded later.
/// Returns the new Arc ID so the frontend can update the sidebar.
#[tauri::command]
pub async fn new_arc(
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    *state.history.lock().await = Vec::new();
    let new_id = chrono::Utc::now()
        .format("arc_%Y%m%d_%H%M%S")
        .to_string();
    *state.active_arc_id.lock().await = new_id.clone();

    if let Some(ref store) = state.arc_store {
        if let Err(e) = store
            .create_arc(
                &new_id,
                "New Arc",
                arcs::ArcSource::UserInput,
            )
            .await
        {
            warn!("Failed to create arc: {e}");
        }
    }

    Ok(new_id)
}

/// Return the current arc's entries for the frontend to render on startup.
///
/// Falls back to in-memory history if the arc store is unavailable.
#[tauri::command]
pub async fn get_arc_history(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<ArcEntryResponse>, String> {
    if let Some(ref store) = state.arc_store {
        let arc_id = state.active_arc_id.lock().await.clone();
        let entries = store
            .load_entries(&arc_id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(entries
            .into_iter()
            .map(|e| ArcEntryResponse {
                id: e.id,
                entry_type: e.entry_type.as_str().to_string(),
                source: e.source,
                content: e.content,
                metadata: e.metadata,
                created_at: e.created_at,
            })
            .collect());
    }

    // Fallback to in-memory history.
    let history = state.history.lock().await;
    Ok(history
        .iter()
        .filter_map(|m| {
            let (role, content) = match (&m.role, &m.content) {
                (Role::User, MessageContent::Text(t)) => ("user", t.clone()),
                (Role::Assistant, MessageContent::Text(t)) => ("assistant", t.clone()),
                _ => return None,
            };
            Some(ArcEntryResponse {
                id: 0,
                entry_type: "message".to_string(),
                source: role.to_string(),
                content,
                metadata: None,
                created_at: String::new(),
            })
        })
        .collect())
}

/// List all arcs with metadata for the sidebar.
#[tauri::command]
pub async fn list_arcs(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<arcs::ArcMeta>, String> {
    if let Some(ref store) = state.arc_store {
        store.list_arcs().await.map_err(|e| e.to_string())
    } else {
        Ok(Vec::new())
    }
}

/// Timeline data: all arcs with their entries for the full graph view.
#[derive(Serialize)]
pub struct TimelineArc {
    #[serde(flatten)]
    pub meta: arcs::ArcMeta,
    pub entries: Vec<ArcEntryResponse>,
}

/// Return all arcs with their entries for the timeline view.
#[tauri::command]
pub async fn get_timeline_data(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<TimelineArc>, String> {
    if let Some(ref store) = state.arc_store {
        let arc_list = store.list_arcs().await.map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        for meta in arc_list {
            let entries = store
                .load_entries(&meta.id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|e| ArcEntryResponse {
                    id: e.id,
                    entry_type: e.entry_type.as_str().to_string(),
                    source: e.source,
                    content: e.content,
                    metadata: e.metadata,
                    created_at: e.created_at,
                })
                .collect();
            result.push(TimelineArc { meta, entries });
        }
        Ok(result)
    } else {
        Ok(Vec::new())
    }
}

/// Switch to a different arc, loading its entries into the in-memory history.
///
/// Returns the loaded entries so the frontend can render them immediately.
#[tauri::command]
pub async fn switch_arc(
    arc_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<ArcEntryResponse>, String> {
    if let Some(ref store) = state.arc_store {
        let entries = store
            .load_entries(&arc_id)
            .await
            .map_err(|e| e.to_string())?;

        // Rebuild in-memory history from message entries.
        let history: Vec<ChatMessage> = entries
            .iter()
            .filter(|e| e.entry_type == arcs::EntryType::Message)
            .map(|e| ChatMessage {
                role: match e.source.as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "system" => Role::System,
                    "tool" => Role::Tool,
                    _ => Role::User,
                },
                content: MessageContent::Text(e.content.clone()),
            })
            .collect();

        *state.history.lock().await = history;
        *state.active_arc_id.lock().await = arc_id.clone();

        // Mark any pending notifications for this arc as read.
        if let Some(ref notifier) = state.notifier {
            notifier.mark_arc_read(&arc_id).await;
        }

        return Ok(entries
            .into_iter()
            .map(|e| ArcEntryResponse {
                id: e.id,
                entry_type: e.entry_type.as_str().to_string(),
                source: e.source,
                content: e.content,
                metadata: e.metadata,
                created_at: e.created_at,
            })
            .collect());
    }
    Ok(Vec::new())
}

/// Rename an arc.
#[tauri::command]
pub async fn rename_arc(
    arc_id: String,
    name: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    if let Some(ref store) = state.arc_store {
        store
            .rename_arc(&arc_id, &name)
            .await
            .map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Delete an arc and all its entries.
///
/// Returns the arc ID of the arc that should become active
/// (the most recent remaining active arc, or a newly created one).
#[tauri::command]
pub async fn delete_arc(
    arc_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    if let Some(ref store) = state.arc_store {
        store
            .delete_arc(&arc_id)
            .await
            .map_err(|e| e.to_string())?;
    }

    // If deleting the active arc, switch to next or create new.
    let current = state.active_arc_id.lock().await.clone();
    if arc_id == current {
        if let Some(ref store) = state.arc_store {
            let all_arcs = store.list_arcs().await.map_err(|e| e.to_string())?;
            let next = all_arcs
                .into_iter()
                .find(|a| a.status == arcs::ArcStatus::Active)
                .map(|a| a.id);
            if let Some(next_id) = next {
                *state.active_arc_id.lock().await = next_id.clone();
                *state.history.lock().await = Vec::new();
                return Ok(next_id);
            }
        }
        // No arcs left, create new.
        let new_id = chrono::Utc::now()
            .format("arc_%Y%m%d_%H%M%S")
            .to_string();
        if let Some(ref store) = state.arc_store {
            let _ = store
                .create_arc(
                    &new_id,
                    "New Arc",
                    arcs::ArcSource::UserInput,
                )
                .await;
        }
        *state.active_arc_id.lock().await = new_id.clone();
        *state.history.lock().await = Vec::new();
        return Ok(new_id);
    }
    Ok(current)
}

/// Return the current active arc ID.
#[tauri::command]
pub async fn get_current_arc(
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    Ok(state.active_arc_id.lock().await.clone())
}

/// Create a new arc branched from an existing parent arc.
///
/// The new arc starts empty but records the parent relationship.
/// Switches the active arc to the new branch.
#[tauri::command]
pub async fn branch_arc(
    parent_arc_id: String,
    name: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    let new_id = chrono::Utc::now()
        .format("arc_%Y%m%d_%H%M%S")
        .to_string();
    if let Some(ref store) = state.arc_store {
        store
            .create_arc_with_parent(
                &new_id,
                &name,
                arcs::ArcSource::UserInput,
                &parent_arc_id,
            )
            .await
            .map_err(|e| e.to_string())?;
    }

    // Switch to the new branch.
    *state.active_arc_id.lock().await = new_id.clone();
    *state.history.lock().await = Vec::new();

    Ok(new_id)
}

/// Merge all entries from a source arc into a target arc.
///
/// The source arc is marked as Merged. If it was the active arc,
/// switches to the target.
#[tauri::command]
pub async fn merge_arcs(
    source_arc_id: String,
    target_arc_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    if let Some(ref store) = state.arc_store {
        store
            .merge_arc(&source_arc_id, &target_arc_id)
            .await
            .map_err(|e| e.to_string())?;
    }

    // If the merged (source) arc was active, switch to target.
    let current = state.active_arc_id.lock().await.clone();
    if current == source_arc_id {
        *state.active_arc_id.lock().await = target_arc_id;
        *state.history.lock().await = Vec::new();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Calendar commands
// ---------------------------------------------------------------------------

/// List calendar events within a time range.
///
/// `start` and `end` are RFC 3339 timestamps. Returns events whose time range
/// overlaps [start, end].
#[tauri::command]
pub async fn list_calendar_events(
    start: String,
    end: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<CalendarEvent>, String> {
    if let Some(ref store) = state.calendar_store {
        store.list_events(&start, &end).await.map_err(|e| e.to_string())
    } else {
        Ok(Vec::new())
    }
}

/// Create a new calendar event.
///
/// The frontend sends a full `CalendarEvent` object (with a pre-generated id).
/// Returns the event back on success.
#[tauri::command]
pub async fn create_calendar_event(
    event: CalendarEvent,
    state: State<'_, AppState>,
) -> std::result::Result<CalendarEvent, String> {
    if let Some(ref store) = state.calendar_store {
        store.create_event(&event).await.map_err(|e| e.to_string())?;
    }
    Ok(event)
}

/// Update an existing calendar event.
#[tauri::command]
pub async fn update_calendar_event(
    event: CalendarEvent,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    if let Some(ref store) = state.calendar_store {
        store.update_event(&event).await.map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Delete a calendar event by id.
#[tauri::command]
pub async fn delete_calendar_event(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    if let Some(ref store) = state.calendar_store {
        store.delete_event(&id).await.map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Notification commands
// ---------------------------------------------------------------------------

/// Mark a notification as seen, cancelling any pending escalation.
#[tauri::command]
pub async fn mark_notification_seen(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<(), String> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid notification ID: {e}"))?;

    if let Some(ref notifier) = state.notifier {
        notifier.mark_seen(uuid).await;
    }
    Ok(())
}

/// Return all notifications, newest first.
#[tauri::command]
pub async fn list_notifications(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<NotificationInfo>, String> {
    if let Some(ref notifier) = state.notifier {
        Ok(notifier.list_notifications().await)
    } else {
        Ok(vec![])
    }
}

/// Mark a single notification as read (alias for mark_seen with a clearer name).
#[tauri::command]
pub async fn mark_notification_read(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<(), String> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid notification ID: {e}"))?;
    if let Some(ref notifier) = state.notifier {
        notifier.mark_read(uuid).await;
    }
    Ok(())
}

/// Mark all notifications as read.
#[tauri::command]
pub async fn mark_all_notifications_read(
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    if let Some(ref notifier) = state.notifier {
        notifier.mark_all_read().await;
    }
    Ok(())
}

/// Delete a single notification.
#[tauri::command]
pub async fn delete_notification(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<(), String> {
    let uuid =
        Uuid::parse_str(&id).map_err(|e| format!("Invalid notification ID: {e}"))?;
    if let Some(ref notifier) = state.notifier {
        notifier.delete_notification(uuid).await;
    }
    Ok(())
}

/// Delete all read notifications. Returns the count of deleted notifications.
#[tauri::command]
pub async fn delete_read_notifications(
    state: State<'_, AppState>,
) -> std::result::Result<usize, String> {
    if let Some(ref notifier) = state.notifier {
        Ok(notifier.delete_read_notifications().await)
    } else {
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// Memory management commands
// ---------------------------------------------------------------------------

/// Serializable memory item for frontend display.
#[derive(Serialize)]
pub struct MemoryInfo {
    pub id: String,
    pub content: String,
    pub source: String,
    pub timestamp: String,
    pub memory_type: String,
}

/// Serializable entity for frontend display.
#[derive(Serialize)]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub metadata: serde_json::Value,
    pub relations: Vec<EntityRelation>,
}

/// A relation shown inline on an entity card.
#[derive(Serialize)]
pub struct EntityRelation {
    pub relation: String,
    pub target_name: String,
    pub direction: String, // "out" or "in"
}

/// Serializable relation for frontend display.
#[derive(Serialize)]
pub struct RelationInfo {
    pub from_id: String,
    pub from_name: String,
    pub relation: String,
    pub to_id: String,
    pub to_name: String,
}

/// List all stored memories.
#[tauri::command]
pub async fn list_memories(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<MemoryInfo>, String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let items = memory.list_all().await.map_err(|e| e.to_string())?;
    Ok(items
        .into_iter()
        .map(|item| MemoryInfo {
            id: item.id,
            content: item.content,
            source: item
                .metadata
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            timestamp: item
                .metadata
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            memory_type: if item.metadata.get("key").is_some() {
                "keyword".to_string()
            } else {
                "semantic".to_string()
            },
        })
        .collect())
}

/// Update a memory item's content.
#[tauri::command]
pub async fn update_memory(
    state: State<'_, AppState>,
    id: String,
    content: String,
) -> std::result::Result<(), String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    memory.update(&id, &content).await.map_err(|e| e.to_string())
}

/// Delete a memory item.
#[tauri::command]
pub async fn delete_memory(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<(), String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    memory.forget(&id).await.map_err(|e| e.to_string())
}

/// List all entities in the knowledge graph.
#[tauri::command]
pub async fn list_entities(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<EntityInfo>, String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let entities = memory.list_entities().await.map_err(|e| e.to_string())?;
    let all_relations = memory.list_relations().await.unwrap_or_default();

    Ok(entities
        .into_iter()
        .map(|e| {
            let eid = e.id.map(|id| id.to_string()).unwrap_or_default();
            // Collect relations where this entity is involved.
            let relations: Vec<EntityRelation> = all_relations
                .iter()
                .filter_map(|(from_id, from_name, relation, to_id, to_name)| {
                    let fid = from_id.to_string();
                    let tid = to_id.to_string();
                    if fid == eid {
                        Some(EntityRelation {
                            relation: relation.clone(),
                            target_name: to_name.clone(),
                            direction: "out".to_string(),
                        })
                    } else if tid == eid {
                        Some(EntityRelation {
                            relation: relation.clone(),
                            target_name: from_name.clone(),
                            direction: "in".to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect();

            EntityInfo {
                id: eid,
                name: e.name,
                entity_type: format!("{:?}", e.entity_type),
                metadata: e.metadata,
                relations,
            }
        })
        .collect())
}

/// List all relations in the knowledge graph.
#[tauri::command]
pub async fn list_relations(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<RelationInfo>, String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let relations = memory.list_relations().await.map_err(|e| e.to_string())?;
    Ok(relations
        .into_iter()
        .map(|(from_id, from_name, relation, to_id, to_name)| RelationInfo {
            from_id: from_id.to_string(),
            from_name,
            relation,
            to_id: to_id.to_string(),
            to_name,
        })
        .collect())
}

/// Update an entity's name and/or type.
#[tauri::command]
pub async fn update_entity(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    entity_type: Option<String>,
) -> std::result::Result<(), String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let entity_id =
        Uuid::parse_str(&id).map_err(|e| format!("Invalid entity ID: {e}"))?;
    let parsed_type = entity_type.map(|t| match t.as_str() {
        "Person" => athen_core::traits::memory::EntityType::Person,
        "Organization" => athen_core::traits::memory::EntityType::Organization,
        "Project" => athen_core::traits::memory::EntityType::Project,
        "Event" => athen_core::traits::memory::EntityType::Event,
        "Document" => athen_core::traits::memory::EntityType::Document,
        _ => athen_core::traits::memory::EntityType::Concept,
    });
    memory
        .update_entity(entity_id, name, parsed_type)
        .await
        .map_err(|e| e.to_string())
}

/// Delete an entity and all its relations.
#[tauri::command]
pub async fn delete_entity(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<(), String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let entity_id =
        Uuid::parse_str(&id).map_err(|e| format!("Invalid entity ID: {e}"))?;
    memory
        .delete_entity(entity_id)
        .await
        .map_err(|e| e.to_string())
}

/// Delete a specific relation between two entities.
#[tauri::command]
pub async fn delete_relation(
    state: State<'_, AppState>,
    from_id: String,
    to_id: String,
    relation: String,
) -> std::result::Result<(), String> {
    let memory = state.memory.as_ref().ok_or("Memory not initialized")?;
    let from =
        Uuid::parse_str(&from_id).map_err(|e| format!("Invalid from entity ID: {e}"))?;
    let to =
        Uuid::parse_str(&to_id).map_err(|e| format!("Invalid to entity ID: {e}"))?;
    memory
        .delete_relation(from, to, &relation)
        .await
        .map_err(|e| e.to_string())
}

// ─── MCP management ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntryView {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub icon: Option<String>,
    pub config_schema: serde_json::Value,
    pub enabled: bool,
    pub config: serde_json::Value,
}

#[tauri::command]
pub async fn list_mcp_catalog(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<CatalogEntryView>, String> {
    let enabled_ids: std::collections::HashSet<String> =
        state.mcp.enabled_ids().await.into_iter().collect();
    let enabled_configs: std::collections::HashMap<String, serde_json::Value> = state
        .mcp
        .enabled_entries()
        .await
        .into_iter()
        .map(|e| (e.entry.id, e.config))
        .collect();
    Ok(athen_mcp::builtin_catalog()
        .into_iter()
        .map(|e| {
            let id = e.id.clone();
            CatalogEntryView {
                enabled: enabled_ids.contains(&id),
                config: enabled_configs
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                id,
                display_name: e.display_name,
                description: e.description,
                icon: e.icon,
                config_schema: e.config_schema,
            }
        })
        .collect())
}

#[tauri::command]
pub async fn enable_mcp(
    state: State<'_, AppState>,
    mcp_id: String,
    config: serde_json::Value,
) -> std::result::Result<(), String> {
    state
        .mcp
        .enable(&mcp_id, config.clone())
        .await
        .map_err(|e| e.to_string())?;
    if let Some(store) = &state.mcp_store {
        store
            .enable(&mcp_id, &config)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Err(e) = state.refresh_tools_doc().await {
        tracing::warn!("Failed to refresh TOOLS.md after enable_mcp: {e}");
    }
    Ok(())
}

#[tauri::command]
pub async fn disable_mcp(
    state: State<'_, AppState>,
    mcp_id: String,
) -> std::result::Result<(), String> {
    state.mcp.disable(&mcp_id).await;
    if let Some(store) = &state.mcp_store {
        store.disable(&mcp_id).await.map_err(|e| e.to_string())?;
    }
    if let Err(e) = state.refresh_tools_doc().await {
        tracing::warn!("Failed to refresh TOOLS.md after disable_mcp: {e}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path-grant approval flow + grant management
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DirectoryGrantSummary {
    pub id: i64,
    pub scope: String,
    pub arc_id: Option<String>,
    pub path: String,
    pub access: String,
}

fn grant_to_summary(g: athen_persistence::grants::DirectoryGrant) -> DirectoryGrantSummary {
    let (scope, arc_id) = match g.scope {
        athen_persistence::grants::GrantScope::Arc(id) => ("arc".to_string(), Some(id.to_string())),
        athen_persistence::grants::GrantScope::Global => ("global".to_string(), None),
    };
    DirectoryGrantSummary {
        id: g.id,
        scope,
        arc_id,
        path: g.path.display().to_string(),
        access: match g.access {
            athen_persistence::grants::Access::Read => "read".to_string(),
            athen_persistence::grants::Access::Write => "write".to_string(),
        },
    }
}

#[tauri::command]
pub async fn list_pending_grants(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<PendingGrantSummary>, String> {
    let map = state.pending_grants.lock().await;
    Ok(map.iter().map(|(id, req)| req.summary(*id)).collect())
}

#[tauri::command]
pub async fn resolve_pending_grant(
    state: State<'_, AppState>,
    id: String,
    decision: GrantDecision,
) -> std::result::Result<(), String> {
    let id: Uuid = id.parse().map_err(|e| format!("Invalid id: {e}"))?;
    let req = {
        let mut map = state.pending_grants.lock().await;
        map.remove(&id)
            .ok_or_else(|| "No such pending grant".to_string())?
    };
    req.responder
        .send(decision)
        .map_err(|_| "Pending grant already resolved".to_string())
}

#[tauri::command]
pub async fn list_arc_grants(
    state: State<'_, AppState>,
    arc_id: String,
) -> std::result::Result<Vec<DirectoryGrantSummary>, String> {
    let store = state
        .grant_store
        .as_ref()
        .ok_or_else(|| "Grant store unavailable".to_string())?;
    let arc_uuid = crate::file_gate::arc_uuid(&arc_id);
    let grants = store
        .list_arc(arc_uuid)
        .await
        .map_err(|e| e.to_string())?;
    Ok(grants.into_iter().map(grant_to_summary).collect())
}

#[tauri::command]
pub async fn list_global_grants(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<DirectoryGrantSummary>, String> {
    let store = state
        .grant_store
        .as_ref()
        .ok_or_else(|| "Grant store unavailable".to_string())?;
    let grants = store
        .list_global()
        .await
        .map_err(|e| e.to_string())?;
    Ok(grants.into_iter().map(grant_to_summary).collect())
}

#[tauri::command]
pub async fn add_global_grant(
    state: State<'_, AppState>,
    path: String,
    access: String,
) -> std::result::Result<(), String> {
    let store = state
        .grant_store
        .as_ref()
        .ok_or_else(|| "Grant store unavailable".to_string())?;
    let access = match access.to_lowercase().as_str() {
        "read" => athen_persistence::grants::Access::Read,
        "write" => athen_persistence::grants::Access::Write,
        other => return Err(format!("Invalid access: {other}")),
    };
    store
        .grant_global(std::path::Path::new(&path), access)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn revoke_arc_grant(
    state: State<'_, AppState>,
    id: i64,
) -> std::result::Result<(), String> {
    let store = state
        .grant_store
        .as_ref()
        .ok_or_else(|| "Grant store unavailable".to_string())?;
    store.revoke_arc_by_id(id).await.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn revoke_global_grant(
    state: State<'_, AppState>,
    id: i64,
) -> std::result::Result<(), String> {
    let store = state
        .grant_store
        .as_ref()
        .ok_or_else(|| "Grant store unavailable".to_string())?;
    store.revoke_global_by_id(id).await.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod key_term_tests {
    use super::extract_key_terms;

    #[test]
    fn basic_extraction() {
        let terms = extract_key_terms("Nadia likes Rust programming");
        assert!(terms.contains(&"Nadia".to_string()));
        assert!(terms.contains(&"likes".to_string()));
        assert!(terms.contains(&"Rust".to_string()));
        assert!(terms.contains(&"programming".to_string()));
    }

    #[test]
    fn stop_words_filtered_spanish_and_english() {
        // Spanish stop words
        let terms = extract_key_terms("el gato está en la casa");
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("el")));
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("la")));
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("en")));
        assert!(terms.contains(&"gato".to_string()));
        assert!(terms.contains(&"casa".to_string()));

        // English stop words
        let terms = extract_key_terms("the cat is on the table");
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("the")));
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("is")));
        assert!(!terms.iter().any(|t| t.eq_ignore_ascii_case("on")));
        assert!(terms.contains(&"cat".to_string()));
        assert!(terms.contains(&"table".to_string()));
    }

    #[test]
    fn short_words_filtered() {
        let terms = extract_key_terms("go do it ox");
        // All words are <= 2 chars → should all be filtered
        assert!(terms.is_empty());
    }

    #[test]
    fn accented_characters_preserved() {
        let terms = extract_key_terms("información está aquí código");
        assert!(terms.contains(&"información".to_string()));
        assert!(terms.contains(&"código".to_string()));
    }

    #[test]
    fn empty_string_returns_empty() {
        let terms = extract_key_terms("");
        assert!(terms.is_empty());
    }

    #[test]
    fn all_stop_words_returns_empty() {
        let terms = extract_key_terms("the and or but not for with");
        assert!(terms.is_empty());
    }
}
