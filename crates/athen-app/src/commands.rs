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

use athen_agent::{AgentBuilder, InMemoryAuditor};
use athen_core::error::Result as AthenResult;
use athen_core::event::*;
use athen_core::llm::{ChatMessage, MessageContent, Role};
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_core::risk::{RiskDecision, RiskLevel};
use athen_core::task::{DomainType, Task, TaskId, TaskPriority, TaskStatus, TaskStep};
use athen_core::traits::agent::{AgentExecutor, StepAuditor};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::memory::MemoryStore;
use athen_persistence::arcs;
use athen_persistence::calendar::CalendarEvent;

use crate::file_gate::{GrantDecision, PendingGrantSummary};
use crate::notifier::NotificationInfo;
use crate::state::{AppState, PendingApproval, SharedRouter};

/// One file the user attached in the chat composer (paperclip / drop /
/// paste). The frontend already has the bytes — this just shuttles
/// them across the IPC boundary; once persisted they flow through the
/// same `AttachmentStore` machinery as inbound email/Telegram files,
/// so `prepare_attachment_surfacing` can inline PDFs and the agent can
/// call `read_attachment_full` / `fetch_attachment` against them.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UploadedAttachment {
    pub name: String,
    pub mime_type: String,
    pub base64: String,
}

/// Fire an approval question through the router so the user can also
/// answer via Telegram (in addition to the in-app card we always show).
///
/// Spawned in the background; the future polls the router and on
/// answer drives the coordinator + emits an event the frontend can
/// react to. Whichever channel responds first wins; the existing
/// `approve_task` Tauri command stays the canonical execution path.
fn spawn_router_approval(
    state: &State<'_, AppState>,
    app_handle: &AppHandle,
    task_id: Uuid,
    description: String,
    risk_score: f64,
    risk_level: String,
) {
    use athen_core::approval::ApprovalQuestion;
    use athen_core::notification::{NotificationOrigin, NotificationUrgency};

    // Skip if the router is not configured (e.g. tests without
    // init_approval_router).
    let Some(router) = state.approval_router.clone() else {
        return;
    };
    // Skip if there's no Telegram sink — nothing extra to gain over the
    // in-app card the frontend is already showing.
    let Some(telegram_sink) = state.telegram_approval_sink.clone() else {
        return;
    };

    let arc_id = state.active_arc_id.try_lock().map(|g| g.clone()).ok();
    let app_handle = app_handle.clone();

    // Resolve per-arc compaction budget from the active provider's
    // `context_window_tokens` × `compaction_trigger_pct` /
    // `compaction_target_pct` once at construction. Snapshot semantics
    // means a mid-task provider switch can't move the goalposts.
    let active_provider_id_snapshot = state
        .active_provider_id
        .try_lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let cfg_for_resolvers = crate::state::load_config();
    let (compaction_trigger_tokens, compaction_target_tokens) =
        crate::compaction::resolve_compaction_budget(
            &cfg_for_resolvers,
            &active_provider_id_snapshot,
        );
    let sampling_temperature = crate::compaction::resolve_provider_temperature(
        &cfg_for_resolvers,
        &active_provider_id_snapshot,
    );

    // Clone every AppState bit the helper would need, so the bg task can
    // drive execution without borrowing `&AppState`.
    let bg_ctx = ApprovedTaskBgCtx {
        coordinator: Arc::clone(&state.coordinator),
        router_arc: Arc::clone(&state.router),
        arc_store: state.arc_store.clone(),
        calendar_store: state.calendar_store.clone(),
        contact_store: state.contact_store.clone(),
        memory: state.memory.clone(),
        mcp: Arc::clone(&state.mcp),
        tool_doc_dir: state.tool_doc_dir.clone(),
        grant_store: state.grant_store.clone(),
        profile_store: state.profile_store.clone(),
        identity_store: state.identity_store.clone(),
        pending_grants: state.pending_grants.clone(),
        spawned_processes: state.spawned_processes.clone(),
        telegram_sink: telegram_sink.clone(),
        cancel_flag: Arc::clone(&state.cancel_flag),
        active_arc_id: arc_id.clone().unwrap_or_default(),
        inflight: state.inflight_approvals.clone(),
        approval_router: state.approval_router.clone(),
        notifier: state.notifier.clone(),
        compactor: state.compactor.clone(),
        web_search: Arc::clone(&state.web_search),
        email_sender: state.email_sender.clone(),
        attachment_store: state.attachment_store(),
        compaction_trigger_tokens,
        compaction_target_tokens,
        sampling_temperature,
        wakeup_store: state
            .wakeup_store
            .clone()
            .map(|s| s as Arc<dyn athen_core::traits::wakeup::WakeupStore>),
    };

    tauri::async_runtime::spawn(async move {
        let prompt = format!("Action requires approval (risk {risk_score:.0}, {risk_level}).");
        let description_opt = if description.is_empty() {
            None
        } else {
            Some(description.clone())
        };
        let question = ApprovalQuestion {
            id: Uuid::new_v4(),
            prompt,
            description: description_opt,
            choices: vec![
                athen_core::approval::ApprovalChoice::approve(),
                athen_core::approval::ApprovalChoice::deny(),
            ],
            arc_id: arc_id.clone(),
            task_id: Some(task_id),
            origin: NotificationOrigin::RiskSystem,
            urgency: NotificationUrgency::High,
            created_at: chrono::Utc::now(),
        };

        // Start by asking on the user's preferred channel for this arc.
        let primary = router.pick_primary(arc_id.as_deref()).await;
        let answer = match router.ask_with_escalation(question, primary).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("Approval router failed for task {task_id}: {e}");
                return;
            }
        };

        let approved = answer.choice_key == "approve";
        // Emit an event the frontend can listen to (e.g. to update the
        // pending-approval card or auto-trigger approve_task).
        let _ = app_handle.emit(
            "approval-resolved",
            serde_json::json!({
                "task_id": task_id.to_string(),
                "choice": answer.choice_key,
                "approved": approved,
            }),
        );

        tracing::info!(
            task_id = %task_id,
            choice = %answer.choice_key,
            "Approval resolved via router"
        );

        if !approved {
            // Deny path: tell the coordinator, no execution.
            // The in-app `approve_task` IPC handler does the same when the
            // user taps Deny in the UI; the inflight guard in
            // execute_approved_task isn't relevant here because we never
            // call it on the deny branch.
            if let Err(e) = bg_ctx.coordinator.deny_task(task_id).await {
                tracing::debug!(
                    task_id = %task_id,
                    "Coordinator deny_task failed (likely already denied via in-app): {e}"
                );
            }
            return;
        }

        // Approved → drive execution end-to-end so the user doesn't need
        // the desktop UI to be open. The inflight guard inside
        // execute_approved_task ensures we no-op cleanly if the in-app
        // IPC `approve_task` already started this task.
        let turn_id = Uuid::new_v4().to_string();
        let ctx = ApprovedTaskCtx {
            coordinator: bg_ctx.coordinator,
            router: bg_ctx.router_arc,
            arc_store: bg_ctx.arc_store,
            calendar_store: bg_ctx.calendar_store,
            contact_store: bg_ctx.contact_store,
            memory: bg_ctx.memory,
            mcp: bg_ctx.mcp,
            tool_doc_dir: bg_ctx.tool_doc_dir,
            grant_store: bg_ctx.grant_store,
            profile_store: bg_ctx.profile_store,
            identity_store: bg_ctx.identity_store,
            pending_grants: bg_ctx.pending_grants,
            spawned_processes: bg_ctx.spawned_processes,
            telegram_approval_sink: Some(bg_ctx.telegram_sink.clone()),
            cancel_flag: bg_ctx.cancel_flag,
            active_arc_id: bg_ctx.active_arc_id,
            inflight: bg_ctx.inflight,
            app_handle: app_handle.clone(),
            turn_id,
            // Bg path doesn't have access to the stashed pending_message
            // (that lives on `&AppState` and only the IPC handler can take
            // it). Falling back to the coordinator task description is
            // what the IPC path also does when nothing is stashed.
            message_override: if description.is_empty() {
                None
            } else {
                Some(description)
            },
            approval_router: bg_ctx.approval_router,
            notifier: bg_ctx.notifier.clone(),
            compactor: bg_ctx.compactor.clone(),
            web_search: bg_ctx.web_search.clone(),
            email_sender: bg_ctx.email_sender.clone(),
            initial_user_images: Vec::new(),
            attachment_store: bg_ctx.attachment_store.clone(),
            compaction_trigger_tokens: bg_ctx.compaction_trigger_tokens,
            compaction_target_tokens: bg_ctx.compaction_target_tokens,
            sampling_temperature: bg_ctx.sampling_temperature,
            // Bg path drives Telegram-originated approvals; composer
            // uploads live on the desktop side, so this turn never has
            // an upload event_id to thread through. Same rationale as
            // `message_override` above.
            upload_event_id: None,
            // Bg approval path is for user-driven HumanConfirm flows, not
            // wake-up fires — the autonomy directive doesn't apply.
            wakeup: None,
            wakeup_store: bg_ctx.wakeup_store,
        };

        let outcome = match execute_approved_task(task_id, ctx).await {
            Ok(Some(o)) => o,
            Ok(None) => {
                // In-app path won the race; the user already sees the
                // result there. Nothing left to do for the bg path.
                return;
            }
            Err(e) => {
                tracing::error!(
                    task_id = %task_id,
                    "Background approved-task execution failed: {e}"
                );
                // Surface the failure on Telegram so the user isn't left
                // wondering why the bot went silent after they tapped
                // Approve.
                let chat_id = telegram_sink.chat_id();
                let token = telegram_sink.bot_token().to_string();
                let msg = format!("Sorry, the approved task failed: {e}");
                if let Err(e2) = crate::state::send_telegram_reply(&token, chat_id, &msg).await {
                    tracing::warn!("Failed to send Telegram failure notice: {e2}");
                }
                return;
            }
        };

        // Reply on Telegram with the result (plus a "Tools used" footer when
        // the agent ran any). The reply is unconditional from the bg path —
        // even when the in-app UI is open, the user just answered through
        // Telegram, so closing the loop on the same channel is the right
        // UX. (If they answered in-app, the inflight guard above already
        // returned None and we never get here.)
        let chat_id = telegram_sink.chat_id();
        let token = telegram_sink.bot_token().to_string();
        let footer = crate::state::build_telegram_tools_footer(&outcome.tool_log);
        let outbound = if footer.is_empty() {
            outcome.content.clone()
        } else {
            format!("{}\n\n{}", outcome.content, footer)
        };
        if let Err(e) = crate::state::send_telegram_reply(&token, chat_id, &outbound).await {
            tracing::warn!(
                task_id = %task_id,
                "Failed to send Telegram approved-task reply: {e}"
            );
        }
    });
}

/// Bag-of-fields used to ferry `AppState` bits into the bg approval
/// waiter. Cheaper to pass than reaching for AppState through Tauri's
/// `State<'_, AppState>` (which isn't `'static`).
struct ApprovedTaskBgCtx {
    coordinator: Arc<athen_coordinador::Coordinator>,
    router_arc: Arc<tokio::sync::RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    arc_store: Option<athen_persistence::arcs::ArcStore>,
    calendar_store: Option<athen_persistence::calendar::CalendarStore>,
    contact_store: Option<athen_persistence::contacts::SqliteContactStore>,
    memory: Option<Arc<athen_memory::Memory>>,
    mcp: Arc<athen_mcp::McpRegistry>,
    tool_doc_dir: Option<std::path::PathBuf>,
    grant_store: Option<Arc<athen_persistence::grants::GrantStore>>,
    profile_store: Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    pending_grants: crate::file_gate::PendingGrants,
    spawned_processes: athen_agent::SpawnedProcessMap,
    telegram_sink: Arc<crate::approval::TelegramApprovalSink>,
    cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    active_arc_id: String,
    inflight: crate::state::InflightApprovals,
    approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    notifier: Option<Arc<crate::notifier::NotificationOrchestrator>>,
    compactor: Option<Arc<dyn athen_core::traits::compaction::ArcCompactor>>,
    web_search: Arc<dyn athen_web::WebSearchProvider>,
    email_sender: Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>,
    attachment_store: Option<athen_persistence::attachments::AttachmentStore>,
    compaction_trigger_tokens: u32,
    compaction_target_tokens: u32,
    /// Active provider's sampling-temperature override. `None` means
    /// "let the provider adapter pick its baked-in default" (currently
    /// 0.7 across OpenAI-compat / DeepSeek). Snapshotted at ctx
    /// construction so a mid-task settings change can't move the
    /// goalposts.
    sampling_temperature: Option<f32>,
    /// Wake-up store, threaded into `ApprovedTaskCtx` so the executor
    /// path can compose `create_wakeup` for the agent (Phase 5).
    wakeup_store: Option<Arc<dyn athen_core::traits::wakeup::WakeupStore>>,
}

/// Resolve the agent profile that should drive execution for a given arc.
///
/// Reads `arcs.active_profile_id` to pick a profile id (or falls back to
/// the seeded default), then resolves the profile's persona templates into
/// a `ResolvedAgentProfile` ready to hand to `AgentBuilder::active_profile`.
///
/// Returns `None` (and logs at debug level) on any error or missing wiring
/// — callers continue without a profile, which preserves today's behavior.
/// This is intentional: profiles are an enhancement, not a precondition,
/// so a corrupt or unset DB row should never break the agent.
async fn resolve_active_profile(
    profile_store: Option<&Arc<athen_persistence::profiles::SqliteProfileStore>>,
    arc_store: Option<&athen_persistence::arcs::ArcStore>,
    arc_id: &str,
) -> Option<athen_core::agent_profile::ResolvedAgentProfile> {
    use athen_core::traits::profile::ProfileStore;

    let pstore = profile_store?;
    let astore = arc_store?;
    let arc_meta = match astore.get_arc(arc_id).await {
        Ok(Some(meta)) => meta,
        Ok(None) => {
            tracing::debug!(arc_id = %arc_id, "no arc row when resolving profile");
            return None;
        }
        Err(e) => {
            tracing::debug!(arc_id = %arc_id, error = %e, "get_arc failed when resolving profile");
            return None;
        }
    };
    let profile = match pstore
        .get_or_default(arc_meta.active_profile_id.as_ref())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "profile lookup failed; falling back to no profile");
            return None;
        }
    };
    let templates = pstore
        .resolve_templates(&profile.persona_template_ids)
        .await
        .unwrap_or_default();
    Some(athen_core::agent_profile::ResolvedAgentProfile {
        profile,
        persona_templates: templates,
    })
}

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

/// Public wrapper around [`simplify_error`] for callers in sibling
/// modules (e.g. the Telegram error path in `state.rs`).
pub(crate) fn simplify_error_public(err: &str) -> String {
    simplify_error(err)
}

/// Extract key terms from a user message for broader memory search.
///
/// Filters out common stop words (Spanish + English) and short words,
/// returning meaningful terms that might match stored memories.
fn extract_key_terms(message: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        // Spanish
        "el",
        "la",
        "los",
        "las",
        "un",
        "una",
        "unos",
        "unas",
        "de",
        "del",
        "al",
        "en",
        "con",
        "por",
        "para",
        "que",
        "es",
        "son",
        "fue",
        "ser",
        "estar",
        "haz",
        "hay",
        "tiene",
        "tengo",
        "como",
        "pero",
        "más",
        "muy",
        "sin",
        "sobre",
        "entre",
        "este",
        "esta",
        "ese",
        "esa",
        "aqui",
        "ahi",
        "aquí",
        "ahí",
        "donde",
        "cuando",
        "quien",
        "cual",
        "todo",
        "toda",
        "todos",
        "mi",
        "tu",
        "su",
        "nos",
        "les",
        "me",
        "te",
        "se",
        "lo",
        "le",
        "quiero",
        "puedes",
        "puede",
        "hacer",
        "dime",
        "dame",
        "escribe",
        "escribeme",
        "aqui",
        "chat",
        "algo",
        // English
        "the",
        "a",
        "an",
        "is",
        "are",
        "was",
        "were",
        "be",
        "been",
        "being",
        "have",
        "has",
        "had",
        "do",
        "does",
        "did",
        "will",
        "would",
        "could",
        "should",
        "may",
        "might",
        "can",
        "shall",
        "to",
        "of",
        "in",
        "for",
        "on",
        "with",
        "at",
        "by",
        "from",
        "and",
        "or",
        "but",
        "not",
        "no",
        "my",
        "your",
        "his",
        "her",
        "its",
        "our",
        "their",
        "this",
        "that",
        "what",
        "which",
        "who",
        "how",
        "when",
        "where",
        "why",
        "all",
        "each",
        "me",
        "you",
        "him",
        "it",
        "us",
        "them",
        "some",
        "any",
    ];

    message
        .split(|c: char| {
            !c.is_alphanumeric()
                && c != 'á'
                && c != 'é'
                && c != 'í'
                && c != 'ó'
                && c != 'ú'
                && c != 'ñ'
                && c != 'ü'
        })
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
    memory: &dyn athen_core::traits::memory::MemoryStore,
    user_msg: &str,
    assistant_msg: &str,
) -> Option<String> {
    use athen_core::llm::{
        ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent, ModelProfile,
        Role as LlmRole,
    };

    let existing_block = match memory.recall(user_msg, 3).await {
        Ok(items) if !items.is_empty() => {
            let lines = items
                .iter()
                .map(|m| format!("- {}", m.content))
                .collect::<Vec<_>>()
                .join("\n");
            format!("\n\nMEMORIES ALREADY STORED that may overlap:\n{lines}\n")
        }
        _ => String::new(),
    };

    let prompt = format!(
        "Analyze this conversation exchange and decide if it contains information worth remembering for future conversations.\n\n\
         User: {user_msg}\n\
         Assistant: {assistant_msg}\
         {existing_block}\n\n\
         Worth remembering: facts about people, preferences, relationships, decisions, plans, \
         important events, personal details the user shared, or things the user explicitly asked to remember.\n\
         NOT worth remembering: greetings, small talk, questions about capabilities, \
         generic requests (write a poem, translate), or information the assistant already has from tools, \
         OR anything already covered by the memories listed above.\n\n\
         If worth remembering AND not already covered above, respond with ONLY a concise summary of the new facts (1-2 sentences, no fluff).\n\
         If NOT worth remembering, or if everything is already covered above, respond with exactly: SKIP"
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
    )
    .await
    {
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
///
/// `turn_id` groups this entry with the rest of the conversation turn (user
/// message + tool calls + assistant reply) so the UI can render them together.
async fn persist_entry(
    state: &AppState,
    source: &str,
    content: &str,
    entry_type: &str,
    metadata: Option<serde_json::Value>,
    turn_id: Option<&str>,
) {
    if let Some(ref store) = state.arc_store {
        let arc_id = state.active_arc_id.lock().await.clone();
        let et = arcs::EntryType::from_str(entry_type);
        if let Err(e) = store
            .add_entry(&arc_id, et, source, content, metadata, turn_id)
            .await
        {
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
    pub turn_id: Option<String>,
}

impl From<arcs::ArcEntry> for ArcEntryResponse {
    fn from(e: arcs::ArcEntry) -> Self {
        Self {
            id: e.id,
            entry_type: e.entry_type.as_str().to_string(),
            source: e.source,
            content: e.content,
            metadata: e.metadata,
            created_at: e.created_at,
            turn_id: e.turn_id,
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
///
/// `arc_id` carries the arc this step belongs to so the frontend can
/// drop events that don't match the currently-viewed arc. Without it,
/// progress from a Telegram-driven background arc renders into whatever
/// arc the user happens to be looking at — a real bug we hit when an
/// inbound message creates a new arc while the user is on a different one.
/// `arc_id` is required (non-Option) as a regression guard: callers without
/// an arc must skip the emit entirely rather than send a contextless event.
#[derive(Clone, Serialize)]
pub(crate) struct AgentProgress {
    pub step: u32,
    pub tool_name: String,
    pub status: String,
    /// Tool arguments or result summary (truncated to ~200 chars).
    pub detail: Option<String>,
    pub arc_id: String,
    /// Full tool arguments + result, included so the live UI can build
    /// the same expandable body (Edit diff, Read content, Fetch page, …)
    /// without waiting for an arc reload. `None` on non-tool steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Shared list of tool names that completed successfully during a turn.
///
/// The `TauriAuditor` appends to it as steps finish; callers that need a
/// post-execute summary (e.g. the Telegram handler appending a "Tools used"
/// footer) hold a clone and read after `executor.execute` returns.
pub(crate) type ToolLog = Arc<std::sync::Mutex<Vec<String>>>;

pub(crate) fn new_tool_log() -> ToolLog {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Step auditor that emits Tauri events for real-time progress in the UI and
/// also persists each completed tool invocation to the active arc.
///
/// Tool calls are written one row per invocation, sharing a `turn_id` with the
/// surrounding user/assistant messages so the frontend can group them under
/// the assistant message that owns them.
pub(crate) struct TauriAuditor {
    inner: InMemoryAuditor,
    app_handle: AppHandle,
    arc_store: Option<arcs::ArcStore>,
    arc_id: String,
    turn_id: String,
    tool_log: ToolLog,
    /// When false, the auditor still persists tool_call rows to `arc_entries`
    /// but skips emitting `agent-progress` events. Used for sub-agents spawned
    /// by `delegate_to_agent` so their step-by-step progress doesn't leak into
    /// the parent arc's progress UI.
    emit_progress: bool,
    /// Live progress reporter for owner Telegram turns. When set, the
    /// auditor edits a single status message in place as new tools fire,
    /// so the user isn't watching dead air on a long task. `None` for
    /// in-app turns (the frontend already renders progress directly).
    telegram_progress: Option<Arc<crate::telegram_progress::TelegramProgressReporter>>,
}

impl TauriAuditor {
    pub(crate) fn new(
        app_handle: AppHandle,
        arc_store: Option<arcs::ArcStore>,
        arc_id: String,
        turn_id: String,
        tool_log: ToolLog,
    ) -> Self {
        Self {
            inner: InMemoryAuditor::new(),
            app_handle,
            arc_store,
            arc_id,
            turn_id,
            tool_log,
            emit_progress: true,
            telegram_progress: None,
        }
    }

    /// Like [`Self::new`] but skips emitting `agent-progress` events. Tool
    /// calls are still persisted to `arc_entries` for the given `arc_id` so
    /// the frontend can render them inline later — but the parent UI's live
    /// progress feed isn't polluted with the sub-agent's intermediate steps.
    pub(crate) fn new_silent(
        app_handle: AppHandle,
        arc_store: Option<arcs::ArcStore>,
        arc_id: String,
        turn_id: String,
        tool_log: ToolLog,
    ) -> Self {
        Self {
            inner: InMemoryAuditor::new(),
            app_handle,
            arc_store,
            arc_id,
            turn_id,
            tool_log,
            emit_progress: false,
            telegram_progress: None,
        }
    }

    /// Attach a live Telegram progress reporter. When set, the auditor
    /// pushes each newly-started tool name into the reporter so it can
    /// edit its status message in place.
    pub(crate) fn with_telegram_progress(
        mut self,
        reporter: Arc<crate::telegram_progress::TelegramProgressReporter>,
    ) -> Self {
        self.telegram_progress = Some(reporter);
        self
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

/// Build a one-line summary for a tool call based on its arguments and
/// (optionally) its result. The intent — what the agent did — lives in
/// the args, so we lean on those first; the result fills in
/// confirmations like "wrote 432B" or `next_fire_at`.
///
/// Returns `None` when neither side produces something meaningful; the
/// caller can then fall back to the raw output blob.
pub(crate) fn summarize_tool_call(
    tool: &str,
    args: Option<&serde_json::Value>,
    result: Option<&serde_json::Value>,
) -> Option<String> {
    let s_str = |v: Option<&serde_json::Value>, k: &str| -> Option<String> {
        v.and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let s_u64 = |v: Option<&serde_json::Value>, k: &str| -> Option<u64> {
        v.and_then(|v| v.get(k)).and_then(|v| v.as_u64())
    };

    match tool {
        "read" => {
            let path = s_str(args, "path")?;
            let offset = s_u64(args, "offset");
            let limit = s_u64(args, "limit");
            match (offset, limit) {
                (Some(o), Some(l)) => Some(format!("{path} (lines {o}–{})", o + l)),
                (Some(o), None) => Some(format!("{path} (from line {o})")),
                (None, Some(l)) => Some(format!("{path} (first {l} lines)")),
                (None, None) => Some(path),
            }
        }
        "write" => {
            let path = s_str(args, "path")?;
            if let Some(bytes) = s_u64(result, "bytes_written") {
                Some(format!("{path} ({})", human_bytes(bytes)))
            } else {
                Some(path)
            }
        }
        "edit" => {
            let path = s_str(args, "path")?;
            if let Some(n) = s_u64(result, "replacements") {
                let unit = if n == 1 { "edit" } else { "edits" };
                Some(format!("{path} ({n} {unit})"))
            } else {
                Some(path)
            }
        }
        "list_directory" => {
            let path = s_str(args, "path").unwrap_or_else(|| ".".to_string());
            if let Some(n) = s_u64(result, "count") {
                let unit = if n == 1 { "entry" } else { "entries" };
                Some(format!("{path} ({n} {unit})"))
            } else {
                Some(path)
            }
        }
        "grep" => {
            let pattern = s_str(args, "pattern")?;
            let path = s_str(args, "path").unwrap_or_else(|| ".".to_string());
            let glob = s_str(args, "glob");
            match glob {
                Some(g) => Some(format!("\"{pattern}\" in {path} ({g})")),
                None => Some(format!("\"{pattern}\" in {path}")),
            }
        }
        "shell_execute" | "shell_spawn" => s_str(args, "command"),
        "shell_kill" | "shell_logs" => s_str(args, "pid"),
        "web_search" => s_str(args, "query"),
        "web_fetch" => s_str(args, "url"),
        "email_send" => {
            // Pull from args (richer than result, which echoes only ids).
            let to = args
                .and_then(|v| v.get("to"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let subject = s_str(args, "subject").unwrap_or_default();
            match (to.is_empty(), subject.is_empty()) {
                (true, true) => None,
                (true, false) => Some(subject),
                (false, true) => Some(format!("to {to}")),
                (false, false) => Some(format!("to {to} — {subject}")),
            }
        }
        "memory_store" => s_str(args, "key"),
        "memory_recall" => s_str(args, "key").or_else(|| s_str(args, "query")),
        "calendar_create" => {
            let title = s_str(args, "title").unwrap_or_default();
            let start = s_str(args, "start_time")
                .or_else(|| s_str(args, "start"))
                .unwrap_or_default();
            match (title.is_empty(), start.is_empty()) {
                (true, true) => None,
                (true, false) => Some(start),
                (false, true) => Some(title),
                (false, false) => Some(format!("{title} — {start}")),
            }
        }
        "calendar_list" => {
            let start = s_str(args, "start").or_else(|| s_str(args, "start_date"));
            let end = s_str(args, "end").or_else(|| s_str(args, "end_date"));
            match (start, end) {
                (Some(s), Some(e)) => Some(format!("{s} → {e}")),
                (Some(s), None) => Some(s),
                (None, Some(e)) => Some(format!("until {e}")),
                (None, None) => None,
            }
        }
        "calendar_update" | "calendar_delete" | "contacts_update" | "contacts_delete" => {
            s_str(args, "id")
        }
        "contacts_search" => s_str(args, "query"),
        "contacts_create" => s_str(args, "name"),
        "delegate_to_agent" => {
            let profile = s_str(args, "profile").or_else(|| s_str(args, "agent"));
            let task = s_str(args, "task").or_else(|| s_str(args, "instruction"));
            match (profile, task) {
                (Some(p), Some(t)) => Some(format!("{p}: {t}")),
                (Some(p), None) => Some(p),
                (None, Some(t)) => Some(t),
                (None, None) => None,
            }
        }
        "install_package" | "uninstall_package" => {
            let runtime = s_str(args, "runtime").unwrap_or_default();
            let pkg = s_str(args, "package").unwrap_or_default();
            match (runtime.is_empty(), pkg.is_empty()) {
                (true, true) => None,
                (true, false) => Some(pkg),
                (false, true) => Some(runtime),
                (false, false) => Some(format!("{runtime}: {pkg}")),
            }
        }
        "create_wakeup" => {
            let when = args
                .and_then(|v| v.get("schedule"))
                .map(format_wakeup_when)
                .unwrap_or_else(|| "?".to_string());
            let instruction = s_str(args, "instruction").unwrap_or_default();
            if instruction.is_empty() {
                Some(when)
            } else {
                Some(format!("{when} — {instruction}"))
            }
        }
        _ => None,
    }
}

/// Render a wake-up `schedule` JSON object as a short human label:
/// `in 2h`, `at 2026-05-09 17:00`, `every 1h`, `cron: 0 8 * * *`.
fn format_wakeup_when(schedule: &serde_json::Value) -> String {
    let kind = schedule.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "one_shot" => {
            if let Some(rel) = schedule.get("in").and_then(|v| v.as_str()) {
                return format!("in {rel}");
            }
            if let Some(at) = schedule.get("at").and_then(|v| v.as_str()) {
                let pretty = chrono::DateTime::parse_from_rfc3339(at)
                    .map(|d| {
                        d.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|_| at.to_string());
                return format!("at {pretty}");
            }
            "one-shot".to_string()
        }
        "interval" => match schedule.get("every_seconds").and_then(|v| v.as_u64()) {
            Some(n) => format!("every {}", human_duration(n)),
            None => "interval".to_string(),
        },
        "cron" => {
            let expr = schedule.get("expr").and_then(|v| v.as_str()).unwrap_or("?");
            format!("cron: {expr}")
        }
        other => other.to_string(),
    }
}

fn human_duration(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 && secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
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
        // Prefer args-based summaries — what the agent did (paths, commands,
        // queries) is the user-meaningful part. Fall back to the raw result
        // blob only when no per-tool formatter recognised the call.
        let detail = step.output.as_ref().and_then(|output| {
            if let Some(tool) = output.get("tool").and_then(|t| t.as_str()) {
                let args = output.get("args");
                let result = output.get("result");
                if let Some(s) = summarize_tool_call(tool, args, result) {
                    return Some(Self::truncate_detail(&s, 200));
                }
                if let Some(result) = result {
                    return Some(Self::truncate_detail(
                        &serde_json::to_string(result).unwrap_or_default(),
                        200,
                    ));
                }
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

        if self.emit_progress {
            // Ship args+result+error along with the progress event so the
            // live UI can build the same expandable body the persisted
            // path uses. We deliberately keep these as raw JSON Values
            // (no truncation) — the renderer enforces its own max-height
            // scroll. Pulling them straight from `step.output` avoids
            // re-cloning paths and keeps the event payload aligned with
            // what the auditor will later persist.
            let (args, result, err) = match step.output.as_ref() {
                Some(o) => (
                    o.get("args").cloned(),
                    o.get("result").cloned(),
                    o.get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                ),
                None => (None, None, None),
            };
            let _ = self.app_handle.emit(
                "agent-progress",
                AgentProgress {
                    step: step.index + 1,
                    tool_name: tool_name.clone(),
                    status: format!("{:?}", step.status),
                    detail: detail.clone(),
                    arc_id: self.arc_id.clone(),
                    args,
                    result,
                    error: err,
                },
            );
        }

        // Persist completed tool invocations so the UI can rehydrate them on
        // restart. We only write on terminal states (Completed / Failed) and
        // only when the step carries tool metadata — InProgress events would
        // create duplicate rows for the same invocation.
        if matches!(
            step.status,
            athen_core::task::StepStatus::Completed | athen_core::task::StepStatus::Failed
        ) {
            // Append successful tool names to the shared log so post-execute
            // callers (Telegram footer, future activity feed) can summarize
            // without re-querying SQLite.
            if matches!(step.status, athen_core::task::StepStatus::Completed) {
                if let Some(tool) = step
                    .output
                    .as_ref()
                    .and_then(|o| o.get("tool"))
                    .and_then(|t| t.as_str())
                {
                    if let Ok(mut log) = self.tool_log.lock() {
                        log.push(tool.to_string());
                    }
                }
            }

            // Push tool name to the live Telegram status message so the
            // user watches a growing list instead of dead silence. We
            // include failed tools too — they're informative even when
            // the agent retries — and we run it in a detached task so a
            // Telegram blip can't slow down the executor's audit path.
            if let Some(reporter) = self.telegram_progress.as_ref() {
                if let Some(tool) = step
                    .output
                    .as_ref()
                    .and_then(|o| o.get("tool"))
                    .and_then(|t| t.as_str())
                {
                    let reporter = Arc::clone(reporter);
                    let tool = tool.to_string();
                    tokio::spawn(async move {
                        reporter.report_tool(&tool).await;
                    });
                }
            }

            if let (Some(store), Some(output)) = (self.arc_store.as_ref(), step.output.as_ref()) {
                if let Some(tool) = output.get("tool").and_then(|t| t.as_str()) {
                    let metadata = serde_json::json!({
                        "tool": tool,
                        "args": output.get("args").cloned().unwrap_or(serde_json::Value::Null),
                        "result": output.get("result").cloned().unwrap_or(serde_json::Value::Null),
                        "error": output.get("error").cloned().unwrap_or(serde_json::Value::Null),
                        "status": format!("{:?}", step.status),
                        "summary": detail,
                    });
                    if let Err(e) = store
                        .add_entry(
                            &self.arc_id,
                            arcs::EntryType::ToolCall,
                            "assistant",
                            tool,
                            Some(metadata),
                            Some(&self.turn_id),
                        )
                        .await
                    {
                        warn!("Failed to persist tool_call entry: {e}");
                    }
                }
            }
        }

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

/// Strip path separators and other risky characters out of a
/// user-supplied filename so it can land safely under
/// `<sense-attachments>/<event_id>/<name>`. Mirrors
/// `athen-sentidos::email::sanitize_filename` (kept separate to avoid
/// re-exporting an internal helper).
fn sanitize_upload_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed.to_string()
    }
}

/// Persist composer-uploaded attachments to disk + AttachmentStore.
/// Returns the synthesized `event_id` on success; the caller stamps it
/// onto the user-message arc entry so `latest_sense_event_id_in_arc`
/// picks it up for surfacing AND the frontend can hydrate thumbnails
/// on arc reload via `list_attachments_for_event`.
///
/// Note: the legacy "📎 N file(s) uploaded" marker arc entry is no
/// longer written here — the user-bubble itself carries the attachments
/// (renders thumbnails inline) once the caller passes the returned
/// event_id through `persist_entry`'s metadata. `arc_store` is kept in
/// the signature for future re-use but currently unused.
///
/// Returns `Ok(None)` when there's nothing to persist.
async fn persist_uploaded_attachments(
    _arc_store: Option<&athen_persistence::arcs::ArcStore>,
    attachment_store: Option<&athen_persistence::attachments::AttachmentStore>,
    _arc_id: &str,
    uploads: &[UploadedAttachment],
) -> std::result::Result<Option<Uuid>, String> {
    use base64::Engine;

    if uploads.is_empty() {
        return Ok(None);
    }
    let astore = match attachment_store {
        Some(s) => s,
        None => return Ok(None), // CLI / test path: no DB, no persistence.
    };

    let event_id = Uuid::new_v4();
    let root = athen_core::paths::athen_attachments_dir()
        .ok_or_else(|| "no athen data dir".to_string())?
        .join(event_id.to_string());
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| format!("create upload dir: {e}"))?;

    for upload in uploads {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(upload.base64.as_bytes())
            .map_err(|e| format!("invalid base64 for {}: {e}", upload.name))?;
        let safe_name = sanitize_upload_filename(&upload.name);
        let path = root.join(&safe_name);
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        let att = athen_core::event::Attachment::new(
            safe_name.clone(),
            upload.mime_type.clone(),
            bytes.len() as u64,
            Some(path.clone()),
            None, // No source: local upload, never re-fetchable.
        );
        let att_id = att.id;
        if let Err(e) = astore.insert(event_id, &att).await {
            tracing::warn!(
                attachment = %safe_name,
                error = %e,
                "Failed to insert composer attachment row"
            );
            continue;
        }

        // Eager PDF extraction on the side, mirroring email persist.
        // Failure is logged but not fatal — the surfacing path will
        // lazy-extract again on first read.
        if upload.mime_type.eq_ignore_ascii_case("application/pdf") {
            let pdf_path = path.clone();
            match tokio::task::spawn_blocking(move || {
                athen_sentidos::pdf_extract::extract_to_sidecar(&pdf_path)
            })
            .await
            {
                Ok(Ok(sidecar)) => {
                    if let Err(e) = astore.record_extracted_text(att_id, sidecar).await {
                        tracing::warn!(
                            attachment_id = %att_id,
                            error = %e,
                            "Failed to record extracted text path for upload"
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        attachment_id = %att_id,
                        error = %e,
                        "PDF extraction failed for composer upload"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        attachment_id = %att_id,
                        error = %e,
                        "PDF extraction join error for composer upload"
                    );
                }
            }
        }
    }

    tracing::info!(
        event_id = %event_id,
        count = uploads.len(),
        "Persisted composer uploads"
    );
    Ok(Some(event_id))
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
    images: Option<Vec<athen_core::llm::ImageInput>>,
    attachments: Option<Vec<UploadedAttachment>>,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> std::result::Result<ChatResponse, String> {
    let images = images.unwrap_or_default();
    let attachments = attachments.unwrap_or_default();
    if !images.is_empty() {
        tracing::info!(
            count = images.len(),
            "send_message: turn includes user-attached images"
        );
        // Pre-flight: if the active provider can't accept images, fail
        // with a clear, actionable message instead of letting each
        // adapter surface its own provider-specific rejection. The
        // toggle in Settings is advisory — DeepSeek's standard chat,
        // plain Ollama, and llama.cpp all reject Multimodal at the
        // adapter level regardless. A vision-capable provider (Claude
        // 3.5+, GPT-4o, Gemini 1.5+) needs to be both *configured* and
        // *active* before we even try.
        let active_id = state.active_provider_id.lock().await.clone();
        let models = crate::settings::load_models_config();
        let active_supports_vision = models
            .providers
            .get(&active_id)
            .is_some_and(|c| c.supports_vision);
        // Adapters that hard-reject multimodal regardless of the user's
        // toggle: DeepSeek's standard chat API and the bare local OpenAI-
        // compat wrappers (Ollama, llama.cpp). Google is a stub. The
        // generic `OpenAiCompatibleProvider` (any other id) *does*
        // serialise images and trusts the supports_vision flag.
        let adapter_can_carry_vision = !matches!(
            active_id.as_str(),
            "deepseek" | "ollama" | "llamacpp" | "google"
        );
        if !(active_supports_vision && adapter_can_carry_vision) {
            return Ok(ChatResponse {
                content: format!(
                    "Your active provider ({active_id}) doesn't accept image input. \
                     Open Settings → LLM Providers and switch to a vision-capable \
                     provider (Claude 3.5+, GPT-4o / GPT-4o-mini, Gemini 1.5+) — \
                     tick the \"Vision-capable model\" box and activate it. Then \
                     reattach the image and send again."
                ),
                risk_level: Some("Caution".into()),
                domain: None,
                tool_calls: vec![],
                pending_approval: None,
            });
        }
    }
    // Stable id for every entry produced by this turn (user msg, tool calls,
    // assistant reply). The frontend groups by this for the dropdown UI.
    let turn_id = Uuid::new_v4().to_string();

    // Record that the user just engaged through the in-app UI on this
    // arc — the approval router will prefer this channel for follow-up
    // questions on the same arc.
    let active_arc = state.active_arc_id.lock().await.clone();
    {
        if let Some(ref store) = state.arc_store {
            if let Err(e) = store.set_primary_reply_channel(&active_arc, "in_app").await {
                tracing::debug!("Failed to update primary_reply_channel: {e}");
            }
        }
    }

    // Persist composer-uploaded files (PDFs, docs, etc.) AND composer
    // images before the executor runs. We unify both into the same
    // AttachmentStore so the surfacing path picks them up uniformly,
    // arc reload can render thumbnails by `attachment_event_id`, and
    // there's a single durable representation of "media the user
    // attached to this turn." Images are converted to UploadedAttachment
    // shape so they share the persist path.
    let mut all_uploads: Vec<UploadedAttachment> =
        Vec::with_capacity(attachments.len() + images.len());
    all_uploads.extend(attachments.iter().cloned());
    for (idx, img) in images.iter().enumerate() {
        let base64_data = match &img.data {
            athen_core::llm::ImageData::Base64 { data } => data.clone(),
            // URL-form composer images aren't produced by today's UI; if a
            // future code path emits one, skip persistence (we'd need to
            // download bytes first to stash them) and let it flow through
            // the live `images` arg only.
            athen_core::llm::ImageData::Url { .. } => continue,
        };
        let ext = match img.mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        };
        all_uploads.push(UploadedAttachment {
            name: format!("pasted-image-{}.{ext}", idx + 1),
            mime_type: img.mime_type.clone(),
            base64: base64_data,
        });
    }

    let attachment_store_handle = state.attachment_store();
    let upload_event_id = match persist_uploaded_attachments(
        state.arc_store.as_ref(),
        attachment_store_handle.as_ref(),
        &active_arc,
        &all_uploads,
    )
    .await
    {
        Ok(eid) => eid,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to persist composer uploads");
            None
        }
    };
    // Persisted images now live in AttachmentStore and the surfacing
    // path will inline them as multimodal — drop the live `images` arg
    // to avoid duplicating each image on the wire (live + surfaced).
    let images: Vec<athen_core::llm::ImageInput> = if upload_event_id.is_some() {
        Vec::new()
    } else {
        images
    };

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
            arc_id: active_arc.clone(),
            args: None,
            result: None,
            error: None,
        },
    );

    // Route the event through the coordinator (risk + queue).
    let task_results = state.coordinator.process_event(event).await.map_err(|e| {
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
                    skip_humanize: false,
                    body_long: None,
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
        // Stash the upload event_id so the approved-task user-message
        // persist can stamp `attachment_event_id` metadata — without
        // this, thumbnails wouldn't hydrate on arc reload after an
        // approval round-trip.
        *state.pending_upload_event_id.lock().await = upload_event_id;

        let approval = PendingApproval {
            task_id: awaiting_task.id.to_string(),
            description: awaiting_task.description.clone(),
            risk_score,
            risk_level: risk_level.clone(),
        };

        // Fire the same question via the approval router so it can also
        // reach the user on Telegram (or any future channel) if they're
        // not at the UI. The in-app card is still shown so the user can
        // also answer there; whichever channel responds first wins.
        spawn_router_approval(
            &state,
            &app_handle,
            awaiting_task.id,
            awaiting_task.description.clone(),
            risk_score,
            risk_level.clone(),
        );

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
            let context = state.history.lock().await.clone();

            // `system_suffix` accumulates host-supplied volatile content
            // (memory recall, attachment summaries) that used to be
            // pushed as mid-stream `Role::System` messages. Strict chat
            // templates (Qwen, Llama) raise on system messages past
            // position 0, so we now fold this content into the leading
            // system message via `AgentBuilder::external_system_suffix`.
            let mut system_suffix = String::new();

            // Auto-inject relevant memories into context. Single full-message
            // recall against the global min_relevance threshold; the prior
            // per-key-term fan-out flooded context with low-confidence hits.
            if let Some(ref memory) = state.memory {
                let mut all_items = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();
                if let Ok(items) = memory.recall(&message, 3).await {
                    for item in items {
                        if seen_ids.insert(item.id.clone()) {
                            all_items.push(item);
                        }
                    }
                }

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
                    // Fold into the leading system message via
                    // `external_system_suffix` instead of pushing as a
                    // mid-stream `Role::System` — strict chat templates
                    // (Qwen, Llama) raise on non-leading system roles.
                    // The executor appends this after its own volatile
                    // state (timestamp), so the static prefix above the
                    // suffix stays byte-identical between turns.
                    system_suffix.push_str(&format!(
                        "MEMORIES ALREADY LOADED FROM YOUR PERSISTENT MEMORY \
                         (treat these as authoritative — do not call memory_recall \
                         to re-fetch the same entities listed below; only call \
                         memory_recall if you need *additional* information not \
                         covered here):\n{memory_text}\n\n"
                    ));
                } else {
                    tracing::debug!("No relevant memories found for query");
                }
            }

            // Persist the user message before the executor runs so its DB id
            // sits *before* any tool_call rows the auditor writes during
            // execution. Otherwise the rehydrated UI shows the tool group
            // above the user bubble that triggered it. When this turn carried
            // composer attachments, stamp the synthesized event_id so (a)
            // `latest_sense_event_id_in_arc` finds it for surfacing and (b)
            // the frontend can hydrate inline thumbnails on arc reload.
            let user_msg_metadata = upload_event_id.map(|eid| {
                serde_json::json!({
                    "attachment_event_id": eid.to_string(),
                    "event_id": eid.to_string(),
                })
            });
            persist_entry(
                &state,
                "user",
                &message,
                "message",
                user_msg_metadata,
                Some(&turn_id),
            )
            .await;

            // Surface attachments tied to the most recent sense event in this
            // arc (composer uploads, inbound email/Telegram with attachments).
            // Without this, a user follow-up like "what does the PDF say?"
            // sees the agent guess at fetch_attachment with no UUID.
            //
            // Embed the surfacing into the user task description rather than
            // pushing as a Role::System message: DeepSeek and other OpenAI-
            // compat providers de-emphasize or merge mid-stream system roles
            // away from the leading system slot, and we kept seeing models
            // reply "I don't see any attachment" even with 6 KB of inlined
            // PDF text further up the wire. Stuffing it into the user turn
            // is universally honored, costs nothing extra, and the arc-
            // persisted user message stays clean (it's persisted as `message`
            // earlier — only the executor sees the enriched description).
            let mut surfaced_images: Vec<athen_core::llm::ImageInput> = Vec::new();
            let mut executor_message = message.clone();
            {
                let arc_store_opt = state.arc_store.as_ref();
                let attachment_store_opt = state.attachment_store();
                if let (Some(arc_store), Some(astore)) =
                    (arc_store_opt, attachment_store_opt.as_ref())
                {
                    let arc_id_for_surface = state.active_arc_id.lock().await.clone();
                    if let Some(event_id) =
                        latest_sense_event_id_in_arc(arc_store, &arc_id_for_surface).await
                    {
                        let router_guard = state.router.read().await;
                        let supports_vision = router_guard.any_provider_supports_vision();
                        let supports_documents = router_guard.any_provider_supports_documents();
                        drop(router_guard);
                        let surfacing = prepare_attachment_surfacing(
                            event_id,
                            astore,
                            supports_vision,
                            supports_documents,
                        )
                        .await;
                        if let Some(msg) = surfacing.system_message {
                            tracing::info!(
                                arc_id = %arc_id_for_surface,
                                event_id = %event_id,
                                images = surfacing.images.len(),
                                surfaced_chars = msg.len(),
                                "Embedding attachment surfacing into direct-dispatch user turn"
                            );
                            executor_message = format!("{msg}\n\n---\n\n{message}");
                        }
                        surfaced_images = surfacing.images;
                    }
                }
            }

            // Build executor with real tool execution (same as athen-cli).
            let exec_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(&state.router)));
            let arc_for_registry = state.active_arc_id.lock().await.clone();
            let registry = state
                .build_tool_registry(&arc_for_registry, Some(app_handle.clone()))
                .await;

            let auditor_arc_id = state.active_arc_id.lock().await.clone();
            let auditor = TauriAuditor::new(
                app_handle.clone(),
                state.arc_store.clone(),
                auditor_arc_id,
                turn_id.clone(),
                new_tool_log(),
            );

            // Set up streaming: forward LLM text chunks to the frontend
            // in real time via Tauri events, tagged with the active arc.
            let current_arc = state.active_arc_id.lock().await.clone();
            let stream_tx = spawn_stream_forwarder(&app_handle, Some(current_arc.clone()));

            // Reset and wire the cancellation flag.
            let cancel_flag = Arc::clone(&state.cancel_flag);
            cancel_flag.store(false, Ordering::Relaxed);

            // Snapshot context for post-response reinforcement.
            let context_snapshot = context.clone();

            let active_profile = resolve_active_profile(
                state.profile_store.as_ref(),
                state.arc_store.as_ref(),
                &current_arc,
            )
            .await;

            // Resolve identity *after* active_profile so we can scope by id.
            // Falls back to the default profile id when the arc has no
            // override — matches `resolve_active_profile`'s own fallback.
            let identity_profile_id = active_profile
                .as_ref()
                .map(|p| p.profile.id.clone())
                .unwrap_or_else(|| athen_core::agent_profile::AgentProfile::DEFAULT_ID.to_string());
            let identity_block = crate::identity_render::render_identity_block(
                state.identity_store.as_ref(),
                &identity_profile_id,
            )
            .await;

            let sampling_temperature = crate::compaction::resolve_provider_temperature(
                &crate::state::load_config(),
                &state.active_provider_id.lock().await.clone(),
            );
            let mut builder = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(registry)
                .auditor(Box::new(auditor))
                .max_steps(50)
                .timeout(Duration::from_secs(300))
                .context_messages(context)
                .stream_sender(stream_tx)
                .cancel_flag(cancel_flag)
                .external_system_suffix(Some(system_suffix))
                .identity_block(identity_block)
                .default_temperature(sampling_temperature);
            if let Some(p) = state.tool_doc_dir.clone() {
                builder = builder.tool_doc_dir(p);
            }
            if let Some(profile) = active_profile {
                builder = builder.active_profile(profile);
            }
            builder = builder
                .toolbox_info(athen_agent::toolbox::ToolboxPromptInfo::load().await)
                .shell_kind(athen_agent::detect_shell_kind().await);
            // Stack composer-attached user images on top of any images
            // surfaced from arc attachments (currently surfaced_images is
            // typically empty for upload-only flows because the upload chip
            // for image MIMEs goes through `composerImages`, but PDFs which
            // are surfaced as text have an empty image list).
            let mut combined_images = images.clone();
            combined_images.extend(surfaced_images);
            if !combined_images.is_empty() {
                builder = builder.initial_user_images(combined_images);
            }
            let executor = builder.build().map_err(|e| {
                let raw = e.to_string();
                tracing::error!("AgentBuilder failed: {raw}");
                format_user_error(&raw)
            })?;

            // Create a task for the executor with the user's message. Uses
            // `executor_message` (= message + optional surfacing prelude),
            // not the bare `message` — the arc-persisted version already
            // wrote the bare `message` above, so the surfacing only travels
            // to the LLM, not to the user-visible history.
            let task = Task {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                source_event: None,
                domain: DomainType::Base,
                description: executor_message,
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
                    // User msg was already persisted before the executor ran.
                    persist_entry(&state, "assistant", &msg, "message", None, Some(&turn_id)).await;
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
            // User msg was already persisted before the executor ran.
            persist_entry(
                &state,
                "assistant",
                &content,
                "message",
                None,
                Some(&turn_id),
            )
            .await;

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
                    match judge_worth_remembering(
                        &router,
                        memory_clone.as_ref(),
                        &msg_clone,
                        &content_clone,
                    )
                    .await
                    {
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
                risk_level: Some(if result.success { "Safe" } else { "Caution" }.into()),
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
    // Stable id for the user/tool/assistant entries this approval will produce.
    let turn_id = Uuid::new_v4().to_string();

    let task_uuid: Uuid = task_id
        .parse()
        .map_err(|e| format!("Invalid task ID: {e}"))?;

    if !approved {
        // Deny the task.
        state.coordinator.deny_task(task_uuid).await.map_err(|e| {
            let raw = e.to_string();
            tracing::error!("Deny task failed: {raw}");
            format_user_error(&raw)
        })?;

        // Clear the stashed message + any upload tied to it.
        *state.pending_message.lock().await = None;
        *state.pending_upload_event_id.lock().await = None;

        // Notify the frontend that the resolution happened in-app, so
        // any router-driven Telegram waiter can be cancelled.
        let _ = app_handle.emit(
            "approval-resolved",
            serde_json::json!({
                "task_id": task_uuid.to_string(),
                "choice": "deny",
                "approved": false,
            }),
        );

        return Ok(ChatResponse {
            content: "Action denied. The task has been cancelled.".into(),
            risk_level: Some("Safe".into()),
            domain: None,
            tool_calls: vec![],
            pending_approval: None,
        });
    }

    // Take the stashed pending_message (if any) and let the helper resolve
    // a fallback from the coordinator task description.
    let message_override = state.pending_message.lock().await.take();
    let upload_event_id = state.pending_upload_event_id.lock().await.take();

    let active_arc = state.active_arc_id.lock().await.clone();
    let active_provider_id_snapshot = state.active_provider_id.lock().await.clone();
    let cfg_for_resolvers = crate::state::load_config();
    let (compaction_trigger_tokens, compaction_target_tokens) =
        crate::compaction::resolve_compaction_budget(
            &cfg_for_resolvers,
            &active_provider_id_snapshot,
        );
    let sampling_temperature = crate::compaction::resolve_provider_temperature(
        &cfg_for_resolvers,
        &active_provider_id_snapshot,
    );

    let ctx = ApprovedTaskCtx {
        coordinator: Arc::clone(&state.coordinator),
        router: Arc::clone(&state.router),
        arc_store: state.arc_store.clone(),
        calendar_store: state.calendar_store.clone(),
        contact_store: state.contact_store.clone(),
        memory: state.memory.clone(),
        mcp: Arc::clone(&state.mcp),
        tool_doc_dir: state.tool_doc_dir.clone(),
        grant_store: state.grant_store.clone(),
        profile_store: state.profile_store.clone(),
        identity_store: state.identity_store.clone(),
        pending_grants: state.pending_grants.clone(),
        spawned_processes: state.spawned_processes.clone(),
        telegram_approval_sink: state.telegram_approval_sink.clone(),
        cancel_flag: Arc::clone(&state.cancel_flag),
        active_arc_id: active_arc,
        inflight: state.inflight_approvals.clone(),
        app_handle: app_handle.clone(),
        turn_id: turn_id.clone(),
        message_override,
        approval_router: state.approval_router.clone(),
        notifier: state.notifier.clone(),
        compactor: state.compactor.clone(),
        web_search: Arc::clone(&state.web_search),
        email_sender: state.email_sender.clone(),
        // Approved-via-card path: original images aren't restashed yet
        // (Phase 2 will mirror `pending_message` for images). For now,
        // images flow through the direct-execution path in `send_message`,
        // not through the explicit-approval card.
        initial_user_images: Vec::new(),
        attachment_store: state.attachment_store(),
        compaction_trigger_tokens,
        compaction_target_tokens,
        sampling_temperature,
        upload_event_id,
        // User-driven approved-task path; wake-up directives don't apply.
        wakeup: None,
        wakeup_store: state
            .wakeup_store
            .clone()
            .map(|s| s as Arc<dyn athen_core::traits::wakeup::WakeupStore>),
    };

    let outcome = match execute_approved_task(task_uuid, ctx).await {
        Ok(Some(o)) => o,
        // The other channel (Telegram) already drove this task to completion
        // — nothing to return to the UI; fast-path a placeholder so the
        // frontend's pending card clears.
        Ok(None) => {
            return Ok(ChatResponse {
                content: "Task already handled via another channel.".into(),
                risk_level: Some("Safe".into()),
                domain: None,
                tool_calls: vec![],
                pending_approval: None,
            });
        }
        Err(e) => return Err(e),
    };

    // Mirror the legacy in-app behaviour: append both the user msg and the
    // assistant reply to the in-memory UI history. The bg path skips this.
    {
        let mut history = state.history.lock().await;
        history.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(outcome.message.clone()),
        });
        history.push(ChatMessage {
            role: Role::Assistant,
            content: MessageContent::Text(outcome.content.clone()),
        });
    }

    Ok(ChatResponse {
        content: outcome.content,
        risk_level: Some(if outcome.success { "Safe" } else { "Caution" }.into()),
        domain: Some(format!("{:?}", outcome.domain)),
        tool_calls: vec![],
        pending_approval: None,
    })
}

/// Outcome of [`execute_approved_task`]. The caller decides how to surface
/// `content`/`success`/`domain` (UI response, Telegram reply, …).
pub(crate) struct ApprovedTaskOutcome {
    pub content: String,
    pub success: bool,
    pub domain: DomainType,
    /// The user message that was actually executed (resolved from the
    /// stashed `pending_message` or the task description). Useful for
    /// callers that want to mutate UI history.
    pub message: String,
    /// The chat-history snapshot fed into the executor, including any
    /// memory injection. Callers use this to drive `reinforce_used_memories`.
    #[allow(dead_code)]
    pub context_snapshot: Vec<ChatMessage>,
    /// Tools the agent actually ran. The Telegram-reply path uses this to
    /// build the "Tools used: …" footer.
    pub tool_log: ToolLog,
}

/// Inputs for [`execute_approved_task`]. Bundled into a struct because the
/// helper needs ~15 fields and a positional signature is unreadable.
///
/// All references are owned/Arc-cloned so the helper can be invoked from a
/// `tauri::async_runtime::spawn` closure without borrowing `&AppState`.
pub(crate) struct ApprovedTaskCtx {
    pub coordinator: Arc<athen_coordinador::Coordinator>,
    pub router: Arc<tokio::sync::RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    pub arc_store: Option<athen_persistence::arcs::ArcStore>,
    pub calendar_store: Option<athen_persistence::calendar::CalendarStore>,
    pub contact_store: Option<athen_persistence::contacts::SqliteContactStore>,
    pub memory: Option<Arc<athen_memory::Memory>>,
    pub mcp: Arc<athen_mcp::McpRegistry>,
    pub tool_doc_dir: Option<std::path::PathBuf>,
    pub grant_store: Option<Arc<athen_persistence::grants::GrantStore>>,
    pub profile_store: Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    pub identity_store: Option<Arc<athen_persistence::identity::SqliteIdentityStore>>,
    pub pending_grants: crate::file_gate::PendingGrants,
    pub spawned_processes: athen_agent::SpawnedProcessMap,
    pub telegram_approval_sink: Option<Arc<crate::approval::TelegramApprovalSink>>,
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    pub active_arc_id: String,
    pub inflight: crate::state::InflightApprovals,
    pub app_handle: AppHandle,
    pub turn_id: String,
    /// User message override (typically the stashed `pending_message`); the
    /// helper falls back to the coordinator task description when None.
    pub message_override: Option<String>,
    /// Cross-channel approval router used by the toolbox install gate
    /// (and re-used by file gates, etc.). `None` means no router was
    /// initialized yet, so toolbox installs fail closed.
    pub approval_router: Option<Arc<crate::approval::ApprovalRouter>>,
    /// Notification orchestrator used by `execute_dispatched_task` to fire
    /// the completion ping when an autonomous run finishes. `None` is
    /// tolerated (no ping). User-driven `execute_approved_task` does not
    /// use this — the user is already in the UI and will see the reply.
    pub notifier: Option<Arc<crate::notifier::NotificationOrchestrator>>,
    /// Arc compactor — gateway into arc history for the executor path.
    /// When `Some`, context is built via `load_context_view` (summary +
    /// tail + tool cache). When `None`, the legacy `load_entries`
    /// fallback runs. See `docs/ARC_COMPACTION.md` §8.
    pub compactor: Option<Arc<dyn athen_core::traits::compaction::ArcCompactor>>,
    /// Quota-aware web search backend — Brave → Tavily → DDG-floor chain
    /// built from `config.web_search`. Shared by every executor path so a
    /// rate-limit cooldown discovered on one task carries across to the
    /// next.
    pub web_search: Arc<dyn athen_web::WebSearchProvider>,
    /// Outbound SMTP transport for the `email_send` tool. `None` when
    /// SMTP isn't configured — the tool then refuses with a clear error.
    pub email_sender: Option<Arc<dyn athen_core::traits::email_sender::EmailSender>>,
    /// Images attached to this turn's user message. Empty in the
    /// background/Telegram path; the in-app composer populates this when
    /// the user pastes or drops an image. Forwarded into the executor so
    /// vision-capable LLMs see them on the first turn.
    pub initial_user_images: Vec<athen_core::llm::ImageInput>,
    /// SQLite-backed attachment ref store. The dispatched-task path
    /// queries it by `task.source_event` to inline images / extracted PDF
    /// text into turn 0 by provider capability. `None` when no database
    /// is wired (CLI/test builds) — sense events still execute, but
    /// without attachment surfacing.
    pub attachment_store: Option<athen_persistence::attachments::AttachmentStore>,
    /// Per-arc compaction budget resolved from the active provider's
    /// `context_window_tokens` × `compaction_trigger_pct` /
    /// `compaction_target_pct`. Computed once when the ctx is built so a
    /// mid-task provider switch doesn't change thresholds inside a single
    /// agent run. See `crate::compaction::resolve_compaction_budget`.
    pub compaction_trigger_tokens: u32,
    pub compaction_target_tokens: u32,
    /// Sampling temperature override resolved from the active provider's
    /// `temperature` field. `None` means "use the adapter's baked-in
    /// default". Same snapshot semantics as the compaction budget so a
    /// mid-task settings tweak doesn't change behavior inside one run.
    pub sampling_temperature: Option<f32>,
    /// Synthesized `event_id` for composer uploads carried into this
    /// approved-task turn. Stamped onto the user-message arc entry so
    /// reload-time thumbnail hydration works after an approval round-
    /// trip. `None` for sense-originated tasks and for approved turns
    /// with no composer attachments.
    pub upload_event_id: Option<uuid::Uuid>,
    /// The wake-up that fired this task, if any. Set by the dispatch
    /// loop after looking up `state.task_wakeup_map`. `None` for sense-
    /// originated and user-driven tasks. The executor uses it to
    /// prepend an autonomy directive to the system suffix and (Phase
    /// 3c2) to apply tool / contact allowlists.
    pub wakeup: Option<athen_core::wakeup::Wakeup>,
    /// Wake-up persistence handle. When `Some`, the registry composition
    /// adds the agent-authored `create_wakeup` tool so the agent can
    /// schedule its own follow-ups (Phase 5). `None` in CLI / test
    /// builds — the tool is then hidden from the agent.
    pub wakeup_store: Option<Arc<dyn athen_core::traits::wakeup::WakeupStore>>,
}

/// Drive a risk-flagged task all the way through approval, dispatch,
/// executor build, execution, persistence, and memory reinforcement.
///
/// Returns `Ok(None)` when another channel already started executing this
/// task (the inflight guard caught the second caller) — see
/// [`crate::state::InflightApprovals`] for the dedup contract.
///
/// Does **not** mutate `AppState::history` (the in-memory UI history).
/// Foreground callers can append to history themselves after this returns;
/// background callers (Telegram path) intentionally skip that step because
/// when the UI is closed the in-memory history is irrelevant — the SQLite
/// arc is the source of truth on next load.
#[allow(clippy::too_many_lines)]
pub(crate) async fn execute_approved_task(
    task_uuid: Uuid,
    ctx: ApprovedTaskCtx,
) -> std::result::Result<Option<ApprovedTaskOutcome>, String> {
    use athen_core::traits::agent::AgentExecutor;

    // Dedup against the parallel approval channel. Whichever caller (in-app
    // IPC or router-spawned bg waiter) inserts first owns this approval;
    // the other no-ops cleanly. Without this both channels would race the
    // coordinator + executor, double-charging the user and posting two
    // assistant replies.
    {
        let mut inflight = ctx.inflight.lock().await;
        if !inflight.insert(task_uuid) {
            tracing::debug!(
                task_id = %task_uuid,
                "Skipping approved-task execution: already running on another channel"
            );
            return Ok(None);
        }
    }

    // RAII-ish: ensure we always remove from the inflight set on exit.
    struct InflightGuard {
        set: crate::state::InflightApprovals,
        task_id: Uuid,
    }
    impl Drop for InflightGuard {
        fn drop(&mut self) {
            let set = self.set.clone();
            let id = self.task_id;
            tokio::spawn(async move {
                set.lock().await.remove(&id);
            });
        }
    }
    let _guard = InflightGuard {
        set: ctx.inflight.clone(),
        task_id: task_uuid,
    };

    // Approve the task: move it to Pending and enqueue.
    let approved_task = ctx.coordinator.approve_task(task_uuid).await.map_err(|e| {
        let raw = e.to_string();
        tracing::error!("Approve task failed: {raw}");
        format_user_error(&raw)
    })?;

    let message = ctx
        .message_override
        .clone()
        .unwrap_or_else(|| approved_task.description.clone());

    // Dispatch the now-enqueued task.
    let coord_task_id = match ctx.coordinator.dispatch_next().await {
        Ok(Some((id, _))) => id,
        Ok(None) => {
            return Ok(Some(ApprovedTaskOutcome {
                content: "Task approved but no agent is available. Please try again.".into(),
                success: false,
                domain: approved_task.domain.clone(),
                message,
                context_snapshot: vec![],
                tool_log: new_tool_log(),
            }));
        }
        Err(e) => {
            let raw = e.to_string();
            tracing::error!("Dispatch failed (approval): {raw}");
            return Err(format_user_error(&raw));
        }
    };

    // Build context. Routed through the compactor when available — the
    // executor must never read raw `arc_entries` directly (see
    // `docs/ARC_COMPACTION.md` §8 "the discipline rule"). Fall back to
    // load_entries only when no compactor is wired (legacy boot/tests).
    //
    // The compactor returns `(tail messages, system suffix)`: the suffix
    // (compaction summary + tool-result cache) used to be a pair of
    // mid-stream `Role::System` messages but now folds into the leading
    // system message via `external_system_suffix` so strict chat
    // templates (Qwen, Llama) accept it.
    let (context, compaction_suffix): (Vec<ChatMessage>, String) = if let Some(ref compactor) =
        ctx.compactor
    {
        match compactor
            .prepare_context(
                &ctx.active_arc_id,
                ctx.compaction_trigger_tokens,
                ctx.compaction_target_tokens,
            )
            .await
        {
            Ok(view) => crate::compaction::view_to_messages(&view),
            Err(e) => {
                tracing::warn!(arc = %ctx.active_arc_id, error = %e, "compactor.prepare_context failed; using empty context");
                (Vec::new(), String::new())
            }
        }
    } else if let Some(ref store) = ctx.arc_store {
        let messages = match store.load_entries(&ctx.active_arc_id).await {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.entry_type == athen_persistence::arcs::EntryType::Message)
                .filter_map(|e| {
                    let role = match e.source.as_str() {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "system" => Role::System,
                        "tool" => Role::Tool,
                        _ => return None,
                    };
                    Some(ChatMessage {
                        role,
                        content: MessageContent::Text(e.content),
                    })
                })
                .collect(),
            Err(_) => vec![],
        };
        (messages, String::new())
    } else {
        (vec![], String::new())
    };

    // `system_suffix` accumulates host-supplied volatile content that
    // used to ride as mid-stream `Role::System` messages. Strict chat
    // templates (Qwen, Llama) raise on non-leading system roles, so we
    // fold it into the leading system message via
    // `AgentBuilder::external_system_suffix`. Compaction output goes
    // first so the summary precedes memory recall.
    let mut system_suffix = compaction_suffix;

    // Auto-inject relevant memories into context.
    if let Some(ref memory) = ctx.memory {
        let mut all_items = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        if let Ok(items) = memory.recall(&message, 3).await {
            for item in items {
                if seen_ids.insert(item.id.clone()) {
                    all_items.push(item);
                }
            }
        }

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
            // Fold into the leading system message via
            // `external_system_suffix` instead of a mid-stream
            // `Role::System` push — strict chat templates (Qwen, Llama)
            // raise on non-leading system roles.
            system_suffix.push_str(&format!(
                "MEMORIES ALREADY LOADED FROM YOUR PERSISTENT MEMORY \
                 (treat these as authoritative — do not call memory_recall \
                 to re-fetch the same entities listed below; only call \
                 memory_recall if you need *additional* information not \
                 covered here):\n{memory_text}\n\n"
            ));
        }
    }

    // Persist user msg before the executor runs so its DB id sits before
    // any tool_call rows the auditor writes during execution. When this
    // turn carries a composer upload (threaded through approval via
    // `pending_upload_event_id`), stamp the metadata so reload-time
    // thumbnail hydration matches the dispatch path.
    if let Some(ref store) = ctx.arc_store {
        let user_msg_metadata = ctx.upload_event_id.map(|eid| {
            serde_json::json!({
                "attachment_event_id": eid.to_string(),
                "event_id": eid.to_string(),
            })
        });
        if let Err(e) = store
            .add_entry(
                &ctx.active_arc_id,
                athen_persistence::arcs::EntryType::Message,
                "user",
                &message,
                user_msg_metadata,
                Some(&ctx.turn_id),
            )
            .await
        {
            warn!("Failed to persist approved-task user entry: {e}");
        }
        if let Err(e) = store.touch_arc(&ctx.active_arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }

    // Surface attachments tied to the most recent sense event in this
    // arc. Without this, the user typing "what does the PDF say?" in an
    // arc spawned by an email-with-PDF would see the agent guess at
    // `fetch_attachment` with no UUID — the surfacing is what feeds the
    // turn-0 image / inline PDF text + the UUIDs the attachment tools
    // need. Mirrors execute_dispatched_task's surfacing block.
    let mut surfaced_images: Vec<athen_core::llm::ImageInput> = Vec::new();
    if let (Some(arc_store), Some(astore)) = (ctx.arc_store.as_ref(), ctx.attachment_store.as_ref())
    {
        if let Some(event_id) = latest_sense_event_id_in_arc(arc_store, &ctx.active_arc_id).await {
            let router_guard = ctx.router.read().await;
            let supports_vision = router_guard.any_provider_supports_vision();
            let supports_documents = router_guard.any_provider_supports_documents();
            drop(router_guard);
            let surfacing =
                prepare_attachment_surfacing(event_id, astore, supports_vision, supports_documents)
                    .await;
            if let Some(msg) = surfacing.system_message {
                tracing::info!(
                    arc_id = %ctx.active_arc_id,
                    event_id = %event_id,
                    images = surfacing.images.len(),
                    surfaced_chars = msg.len(),
                    context_messages_before = context.len(),
                    "Surfacing attachments to user-chat executor"
                );
                // Fold into the leading system message instead of a
                // mid-stream `Role::System` push (Qwen/Llama Jinja).
                system_suffix.push_str(&msg);
                if !system_suffix.ends_with("\n\n") {
                    system_suffix.push_str("\n\n");
                }
            }
            surfaced_images = surfacing.images;
        }
    }

    // Wake-up autonomy directive. Last block in `system_suffix` so the
    // LLM reads it after compaction summary, memory, and surfaced
    // attachments. Soft enforcement (the LLM must self-restrict) — the
    // hard layer (tool/contact allowlists at registry level) ships in
    // Phase 3c2. Today this is what tells a 3am scheduled job "you are
    // running unattended; be conservative" and makes `NotifyOnly`
    // visible to the model.
    if let Some(ref w) = ctx.wakeup {
        use athen_core::wakeup::AutonomyBand;
        let band_directive = match w.autonomy {
            AutonomyBand::Auto => {
                "AUTONOMY: auto. Run anything below `Critical` risk without \
                 prompting. Pause only on Critical actions and write a clear \
                 stop note to the arc."
            }
            AutonomyBand::SafeOnly => {
                "AUTONOMY: safe_only (default). Execute below-threshold \
                 actions without prompting. For anything Caution or above, \
                 stop and write a stop note to the arc — the user will see it \
                 next time they open Athen."
            }
            AutonomyBand::NotifyOnly => {
                "AUTONOMY: notify_only. You may read, summarize, and write to \
                 this arc. You MUST NOT send any outbound message (email, \
                 Telegram, etc.), call any contact, or trigger any external \
                 side effect. If the instruction implies an outbound action, \
                 stop, write what you would have sent into the arc, and exit."
            }
        };
        let header = format!(
            "[Wake-up trigger — id {}, fired by scheduler at {}]\n\
             You were not invoked by a live user; this run is unattended. \
             Output destination is governed by the instruction itself \
             (write to file / send / append). The user will review the arc \
             when they next open Athen.\n\n{band_directive}\n\n",
            w.id,
            chrono::Utc::now().to_rfc3339()
        );
        if !system_suffix.is_empty() && !system_suffix.ends_with("\n\n") {
            system_suffix.push_str("\n\n");
        }
        system_suffix.push_str(&header);
    }

    // Build the tool registry, mirroring AppState::build_tool_registry —
    // inlined here because the bg path doesn't own `&AppState`.
    let mut shell_registry = athen_agent::ShellToolRegistry::new()
        .await
        .with_spawned_processes(ctx.spawned_processes.clone())
        .with_web_search(Arc::clone(&ctx.web_search))
        .with_email_sender_opt(ctx.email_sender.clone());
    if let Some(ref store) = ctx.grant_store {
        let provider = Arc::new(crate::file_gate::ArcWritableProvider {
            arc_id: crate::file_gate::arc_uuid(&ctx.active_arc_id),
            store: store.clone(),
        });
        shell_registry = shell_registry.with_extra_writable(provider);
    }
    if let Some(ref router) = ctx.approval_router {
        shell_registry = shell_registry.with_toolbox_approval(Arc::new(
            crate::file_gate::RouterToolboxApprovalGate::new(
                Arc::clone(router),
                Some(ctx.active_arc_id.clone()),
            ),
        ));
        let gate: Arc<dyn athen_agent::EmailSendApprovalGate> =
            Arc::new(crate::email_gate::RouterEmailApprovalGate::new(
                Arc::clone(router),
                Some(ctx.active_arc_id.clone()),
            ));
        shell_registry = shell_registry.with_email_approval(gate);
    }
    let mut registry = crate::app_tools::AppToolRegistry::new(
        shell_registry,
        ctx.calendar_store.clone(),
        ctx.contact_store.clone(),
        ctx.memory.clone(),
    )
    .with_mcp(ctx.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
    if let Some(ref astore) = ctx.attachment_store {
        registry = registry.with_attachments(astore.clone());
    }
    if let Some(ref store) = ctx.grant_store {
        let mut gate = crate::file_gate::FileGate::new(
            ctx.active_arc_id.clone(),
            store.clone(),
            ctx.pending_grants.clone(),
            Some(ctx.app_handle.clone()),
        );
        if let Some(ref sink) = ctx.telegram_approval_sink {
            gate = gate.with_telegram_approval(sink.clone());
        }
        registry = registry.with_file_gate(Arc::new(gate));
    }

    // Pre-resolve wake-up restrictions once so we can share the same
    // snapshot between the parent's WakeupRestrictedRegistry wrapper and
    // any sub-agent the delegation tool spawns under
    // `inherit_restrictions = true`. Resolution lives here (not inside
    // delegation.rs) because the contact_store lives on AppState — the
    // delegation crate stays free of state plumbing.
    let wakeup_restrictions: Option<crate::wakeup_registry::WakeupSubagentRestrictions> =
        if let Some(ref w) = ctx.wakeup {
            let contact_store_dyn: Option<Arc<dyn athen_contacts::ContactStore>> = ctx
                .contact_store
                .as_ref()
                .map(|s| Arc::new(s.clone()) as Arc<dyn athen_contacts::ContactStore>);
            Some(
                crate::wakeup_registry::resolve_wakeup_restrictions(
                    w.tool_allowlist.clone(),
                    w.contact_allowlist.as_deref(),
                    w.autonomy,
                    contact_store_dyn.as_ref(),
                )
                .await,
            )
        } else {
            None
        };
    // Sub-agent inheritance: only propagate when the wake-up opted in.
    // `inherit_restrictions = false` lets a delegated specialist run with
    // its profile's natural tool surface (e.g. coder needing Tier 2 tools
    // beyond what the wake-up itself declared).
    let subagent_inherit = ctx
        .wakeup
        .as_ref()
        .map(|w| w.inherit_restrictions)
        .unwrap_or(false);
    let subagent_restrictions = if subagent_inherit {
        wakeup_restrictions.clone()
    } else {
        None
    };

    // Wrap in DelegationToolRegistry so the agent can spawn specialists.
    // Sub-agents receive the bare AppToolRegistry — depth=1 by composition.
    let base_registry: Arc<dyn athen_core::traits::tool::ToolRegistry> = Arc::new(registry);
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> =
        if let Some(profile_store) = ctx.profile_store.clone() {
            if let Some(arc_store) = ctx.arc_store.clone() {
                let dctx = crate::delegation::DelegationContext {
                    profile_store,
                    identity_store: ctx.identity_store.clone(),
                    arc_store,
                    llm_router: Arc::clone(&ctx.router),
                    parent_arc_id: ctx.active_arc_id.clone(),
                    tool_doc_dir: ctx.tool_doc_dir.clone(),
                    app_handle: Some(ctx.app_handle.clone()),
                    wakeup_restrictions: subagent_restrictions,
                };
                Box::new(crate::delegation::DelegationToolRegistry::new(
                    base_registry,
                    dctx,
                ))
            } else {
                Box::new(crate::delegation::ArcRegistryAdapter(base_registry))
            }
        } else {
            Box::new(crate::delegation::ArcRegistryAdapter(base_registry))
        };

    // Wake-up authoring layer — adds `create_wakeup` so the agent can
    // schedule its own follow-ups. Sits between delegation and the
    // wake-up restriction wrapper so a locked-down wake-up's
    // tool_allowlist can still hide create_wakeup if the user wants.
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> = match ctx.wakeup_store.clone() {
        Some(store) => {
            let wctx = crate::wakeup_tool::WakeupToolContext {
                wakeup_store: store,
                approval_router: ctx.approval_router.clone(),
                parent_arc_id: ctx.active_arc_id.clone(),
            };
            Box::new(crate::wakeup_tool::WakeupAuthoringRegistry::new(
                registry, wctx,
            ))
        }
        None => registry,
    };

    // Wake-up tool/contact allowlist — sits outermost so it can hide
    // tools that any inner layer (delegation, app tools, MCP) exposes.
    // No-op when the task isn't a wake-up fire or when no allowlists
    // are declared and autonomy is permissive. See `wakeup_registry`.
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> = match wakeup_restrictions {
        Some(restrictions) => Box::new(
            crate::wakeup_registry::WakeupRestrictedRegistry::new_with_resolved(
                registry,
                restrictions,
            ),
        ),
        None => registry,
    };

    let exec_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(&ctx.router)));
    let tool_log = new_tool_log();
    let auditor = TauriAuditor::new(
        ctx.app_handle.clone(),
        ctx.arc_store.clone(),
        ctx.active_arc_id.clone(),
        ctx.turn_id.clone(),
        tool_log.clone(),
    );
    let stream_tx = spawn_stream_forwarder(&ctx.app_handle, Some(ctx.active_arc_id.clone()));

    ctx.cancel_flag.store(false, Ordering::Relaxed);

    let context_snapshot = context.clone();

    let active_profile = resolve_active_profile(
        ctx.profile_store.as_ref(),
        ctx.arc_store.as_ref(),
        &ctx.active_arc_id,
    )
    .await;

    let identity_profile_id = active_profile
        .as_ref()
        .map(|p| p.profile.id.clone())
        .unwrap_or_else(|| athen_core::agent_profile::AgentProfile::DEFAULT_ID.to_string());
    let identity_block = crate::identity_render::render_identity_block(
        ctx.identity_store.as_ref(),
        &identity_profile_id,
    )
    .await;

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(registry)
        .auditor(Box::new(auditor))
        .max_steps(50)
        .timeout(Duration::from_secs(300))
        .context_messages(context)
        .stream_sender(stream_tx)
        .cancel_flag(ctx.cancel_flag.clone())
        .external_system_suffix(Some(system_suffix))
        .identity_block(identity_block)
        .default_temperature(ctx.sampling_temperature);
    if let Some(p) = ctx.tool_doc_dir.clone() {
        builder = builder.tool_doc_dir(p);
    }
    if let Some(profile) = active_profile {
        builder = builder.active_profile(profile);
    }
    builder = builder
        .toolbox_info(athen_agent::toolbox::ToolboxPromptInfo::load().await)
        .shell_kind(athen_agent::detect_shell_kind().await);
    // Stack the originally-attached user images (uploaded via the chat
    // composer) on top of any images surfaced from the arc's most recent
    // sense event — both belong on the very first user turn.
    let mut combined_images = ctx.initial_user_images.clone();
    combined_images.extend(surfaced_images);
    if !combined_images.is_empty() {
        builder = builder.initial_user_images(combined_images);
    }
    let executor = builder.build().map_err(|e| {
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
            let _ = ctx.coordinator.complete_task(coord_task_id).await;
            let raw = e.to_string();
            tracing::error!("Agent execution failed after approval: {raw}");
            let msg = format_user_error(&raw);

            if let Some(ref store) = ctx.arc_store {
                if let Err(e) = store
                    .add_entry(
                        &ctx.active_arc_id,
                        athen_persistence::arcs::EntryType::Message,
                        "assistant",
                        &msg,
                        None,
                        Some(&ctx.turn_id),
                    )
                    .await
                {
                    warn!("Failed to persist approved-task error reply: {e}");
                }
                if let Err(e) = store.touch_arc(&ctx.active_arc_id).await {
                    warn!("Failed to touch arc on error path: {e}");
                }
            }

            return Ok(Some(ApprovedTaskOutcome {
                content: msg,
                success: false,
                domain: approved_task.domain.clone(),
                message,
                context_snapshot,
                tool_log,
            }));
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

    // Persist the assistant response.
    if let Some(ref store) = ctx.arc_store {
        if let Err(e) = store
            .add_entry(
                &ctx.active_arc_id,
                athen_persistence::arcs::EntryType::Message,
                "assistant",
                &content,
                None,
                Some(&ctx.turn_id),
            )
            .await
        {
            warn!("Failed to persist approved-task assistant entry: {e}");
        }
        if let Err(e) = store.touch_arc(&ctx.active_arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }

    // Reinforce memories that were actually used in the response.
    if let Some(ref memory) = ctx.memory {
        reinforce_used_memories(memory, &context_snapshot, &content).await;
    }

    // Auto-remember with the LLM judge.
    if let Some(ref memory) = ctx.memory {
        let router = SharedRouter(Arc::clone(&ctx.router));
        let arc_id = ctx.active_arc_id.clone();
        let msg_clone = message.clone();
        let content_clone = content.clone();
        let memory_clone = Arc::clone(memory);
        tokio::spawn(async move {
            match judge_worth_remembering(
                &router,
                memory_clone.as_ref(),
                &msg_clone,
                &content_clone,
            )
            .await
            {
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

    let _ = ctx.coordinator.complete_task(coord_task_id).await;

    // Notify the frontend so the sidebar refreshes (mirrors the Telegram
    // owner-message handler — relevant when the bg path drives this).
    let _ = ctx.app_handle.emit(
        "arc-updated",
        serde_json::json!({ "arc_id": ctx.active_arc_id }),
    );

    Ok(Some(ApprovedTaskOutcome {
        content,
        success: result.success,
        domain: approved_task.domain.clone(),
        message,
        context_snapshot,
        tool_log,
    }))
}

/// Result of preparing per-attachment surfacing for a dispatched task.
/// `images` slot directly into `AgentBuilder::initial_user_images`;
/// `system_message` (when `Some`) is appended to the conversation
/// context as a System turn so the agent sees it on the very first
/// LLM call.
pub(crate) struct AttachmentSurfacing {
    pub(crate) images: Vec<athen_core::llm::ImageInput>,
    pub(crate) system_message: Option<String>,
}

/// Walk an arc's entries newest-first and return the most recent sense
/// `event_id` recorded in entry metadata. Lets the user-chat executor
/// path locate "the email/telegram message I'm asking about" without
/// needing the original task carry a `source_event` (which only the
/// autonomous dispatch path sets).
///
/// Returns `None` if the arc has no sense entries, the metadata column
/// is empty, or the stored `event_id` isn't a parseable UUID.
pub(crate) async fn latest_sense_event_id_in_arc(
    arc_store: &athen_persistence::arcs::ArcStore,
    arc_id: &str,
) -> Option<Uuid> {
    let entries = arc_store.load_entries(arc_id).await.ok()?;
    for entry in entries.into_iter().rev() {
        let Some(meta) = entry.metadata.as_ref() else {
            continue;
        };
        if let Some(id_str) = meta.get("event_id").and_then(|v| v.as_str()) {
            if let Ok(uuid) = Uuid::parse_str(id_str) {
                return Some(uuid);
            }
        }
    }
    None
}

/// Look up attachments for a sense-originated task, read PDF sidecars
/// and image bytes, and shape them into (images-for-multimodal,
/// system-context-string) by provider capability.
///
/// Best-effort: any error (DB lookup, file read, missing sidecar) is
/// logged and skipped. The caller must be tolerant of an empty result
/// — autonomous tasks without attachments are the common case.
pub(crate) async fn prepare_attachment_surfacing(
    event_id: Uuid,
    attachment_store: &athen_persistence::attachments::AttachmentStore,
    supports_vision: bool,
    supports_documents: bool,
) -> AttachmentSurfacing {
    use base64::Engine;

    let atts = match attachment_store.list_for_event(event_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(event_id = %event_id, error = %e, "list_for_event failed");
            return AttachmentSurfacing {
                images: Vec::new(),
                system_message: None,
            };
        }
    };
    if atts.is_empty() {
        return AttachmentSurfacing {
            images: Vec::new(),
            system_message: None,
        };
    }

    // Log every attachment's actual state at surfacing time. Without
    // this, "the PDF didn't reach the agent" looked identical to "the
    // sidecar wasn't extracted" — and the user can't tell the difference
    // without grovelling through SQLite. Print mime + whether bytes are
    // on disk + whether the .txt sidecar exists per row.
    for att in &atts {
        tracing::info!(
            event_id = %event_id,
            attachment_id = %att.id,
            name = %att.name,
            mime = %att.mime_type,
            size_bytes = att.size_bytes,
            has_local_path = att.local_path.is_some(),
            has_extracted_text = att.extracted_text_path.is_some(),
            purged = att.is_purged(),
            "Surfacing attachment row"
        );
    }

    let mut images: Vec<athen_core::llm::ImageInput> = Vec::new();
    let mut header_lines: Vec<String> = Vec::new();
    let mut body_sections: Vec<String> = Vec::new();
    header_lines.push(format!(
        "ATTACHMENTS ARE ALREADY AVAILABLE TO YOU ({} total). The full \
         extracted contents are inlined further down in this same message \
         under \"BEGIN extracted text\" markers — they are verbatim from \
         the file and authoritative. Use them directly to answer the \
         user's request. DO NOT ask the user to upload, paste, or share \
         the file again — you already have it. DO NOT claim you cannot \
         see attachments.",
        atts.len()
    ));

    for att in &atts {
        let mime = att.mime_type.to_ascii_lowercase();
        let is_image = mime.starts_with("image/");
        let is_pdf = mime.starts_with("application/pdf");
        let mut suffix = String::new();

        if is_image {
            if !supports_vision {
                suffix.push_str(
                    " — image, but no vision-capable provider is active; \
                     metadata only",
                );
            } else if let Some(path) = att.local_path.as_ref() {
                match tokio::fs::read(path).await {
                    Ok(bytes) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        images.push(athen_core::llm::ImageInput {
                            mime_type: att.mime_type.clone(),
                            data: athen_core::llm::ImageData::Base64 { data: b64 },
                        });
                        suffix.push_str(" — inlined as multimodal image");
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to read attachment image bytes"
                        );
                        suffix.push_str(
                            " — bytes unreadable on disk; \
                             call fetch_attachment to retry",
                        );
                    }
                }
            } else if att.is_purged() {
                suffix.push_str(" — bytes purged; call fetch_attachment to redownload");
            } else {
                suffix.push_str(" — bytes not on disk; call fetch_attachment");
            }
        } else if is_pdf {
            // Resolve a usable sidecar path. If the row says we have one,
            // use it. If not but the bytes are still on disk, try to
            // extract right now — extraction during email persist can
            // fail or get skipped, and we'd rather burn a few hundred ms
            // here than send the agent metadata-only and have it ask the
            // user to upload the file they already sent.
            let sidecar: Option<std::path::PathBuf> = match att.extracted_text_path.as_ref() {
                Some(p) => Some(p.clone()),
                None => {
                    if let Some(local) = att.local_path.as_ref() {
                        let local_clone = local.clone();
                        match tokio::task::spawn_blocking(move || {
                            athen_sentidos::pdf_extract::extract_to_sidecar(&local_clone)
                        })
                        .await
                        {
                            Ok(Ok(p)) => {
                                tracing::info!(
                                    attachment_id = %att.id,
                                    sidecar = %p.display(),
                                    "Lazy PDF extraction succeeded at surfacing time"
                                );
                                // Best-effort: persist the new sidecar
                                // path back to the row so subsequent
                                // surfacings hit the cached path. Don't
                                // fail surfacing on a DB write error.
                                if let Err(e) = attachment_store
                                    .record_extracted_text(att.id, p.clone())
                                    .await
                                {
                                    tracing::warn!(
                                        attachment_id = %att.id,
                                        error = %e,
                                        "Failed to persist lazy extracted_text_path"
                                    );
                                }
                                Some(p)
                            }
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    attachment_id = %att.id,
                                    error = %e,
                                    "Lazy PDF extraction failed at surfacing time"
                                );
                                None
                            }
                            Err(e) => {
                                tracing::warn!(
                                    attachment_id = %att.id,
                                    error = %e,
                                    "Lazy PDF extraction join error"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    }
                }
            };

            if let Some(text_path) = sidecar {
                match tokio::fs::read_to_string(&text_path).await {
                    Ok(text) => {
                        let snippet = athen_sentidos::pdf_extract::truncate_for_inline(
                            &text,
                            athen_sentidos::pdf_extract::DEFAULT_INLINE_CHAR_BUDGET,
                        );
                        if snippet.truncated {
                            suffix.push_str(&format!(
                                " — PDF text inlined ({} of {} chars); \
                                 call read_attachment_full(\"{}\") for the rest",
                                snippet.text.chars().count(),
                                snippet.total_chars,
                                att.id
                            ));
                        } else {
                            suffix.push_str(&format!(
                                " — PDF text inlined in full ({} chars)",
                                snippet.total_chars
                            ));
                        }
                        body_sections.push(format!(
                            "--- BEGIN extracted text from \"{}\" (id={}) ---\n{}\n\
                             --- END extracted text from \"{}\" ---",
                            att.name, att.id, snippet.text, att.name
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %text_path.display(),
                            error = %e,
                            "Failed to read PDF sidecar"
                        );
                        suffix.push_str(" — PDF sidecar unreadable");
                    }
                }
                if supports_documents {
                    // Note for future iteration: when MessageContent grows
                    // a Document variant, the document-capable branch will
                    // take precedence over text-fallback inlining.
                    suffix.push_str(
                        " (provider supports native PDF blocks; \
                         not yet wired — using text fallback)",
                    );
                }
            } else {
                suffix.push_str(
                    " — PDF without extracted text and no bytes on disk \
                     to extract from; only metadata is available",
                );
            }
        } else {
            suffix.push_str(" — metadata only");
        }

        header_lines.push(format!(
            "- id={} | name=\"{}\" | mime={} | size={}B{}",
            att.id, att.name, att.mime_type, att.size_bytes, suffix
        ));
    }

    let system_message = {
        let mut out = header_lines.join("\n");
        if !body_sections.is_empty() {
            out.push_str("\n\n");
            out.push_str(&body_sections.join("\n\n"));
        }
        out
    };

    tracing::info!(
        event_id = %event_id,
        attachments = atts.len(),
        images = images.len(),
        body_sections = body_sections.len(),
        system_message_chars = system_message.len(),
        "Built attachment surfacing payload"
    );

    AttachmentSurfacing {
        images,
        system_message: Some(system_message),
    }
}

/// Execute a sense-originated task that the autonomous dispatch loop
/// already pulled out of the coordinator queue.
///
/// Mirrors the post-dispatch portion of [`execute_approved_task`] but
/// with three differences:
///
/// 1. We skip `coordinator.approve_task` and `coordinator.dispatch_next` —
///    the dispatch loop already did both. The caller hands us the
///    full `Task` and the arc id resolved from `task_arc_map`.
/// 2. We do NOT persist a "user" message into the arc. The sense_router
///    already wrote a `system` context entry describing the trigger
///    (email body, calendar event, telegram message, ...) when it
///    landed the event in the arc. Persisting another "user" turn would
///    duplicate the trigger description.
/// 3. The agent runs in autonomous mode (`AgentBuilder::autonomous_mode(true)`),
///    so the system prompt warns the LLM there is no live user and
///    steers uncertain actions through the approval router.
///
/// Returns `Ok(None)` if the inflight guard caught a duplicate (some
/// other channel — e.g. the user opening the arc and tapping approve —
/// already started the same task).
#[allow(clippy::too_many_lines)]
pub(crate) async fn execute_dispatched_task(
    task: athen_core::task::Task,
    arc_id: String,
    ctx: ApprovedTaskCtx,
) -> std::result::Result<Option<ApprovedTaskOutcome>, String> {
    use athen_core::traits::agent::AgentExecutor;

    let coord_task_id = task.id;
    let message = task.description.clone();

    // Inflight dedup: same contract as execute_approved_task. If the
    // user happens to tap approve in-app at the exact moment the
    // dispatch loop is firing, only one channel runs the executor.
    {
        let mut inflight = ctx.inflight.lock().await;
        if !inflight.insert(coord_task_id) {
            tracing::debug!(
                task_id = %coord_task_id,
                "Skipping dispatched-task execution: already running on another channel"
            );
            return Ok(None);
        }
    }

    struct InflightGuard {
        set: crate::state::InflightApprovals,
        task_id: Uuid,
    }
    impl Drop for InflightGuard {
        fn drop(&mut self) {
            let set = self.set.clone();
            let id = self.task_id;
            tokio::spawn(async move {
                set.lock().await.remove(&id);
            });
        }
    }
    let _guard = InflightGuard {
        set: ctx.inflight.clone(),
        task_id: coord_task_id,
    };

    // Build context. Routed through the compactor when available — see
    // `docs/ARC_COMPACTION.md` §8 ("the discipline rule"). Fall back to
    // load_entries only when no compactor is wired.
    // The compactor returns `(tail messages, system suffix)`: the
    // suffix (compaction summary + tool-result cache) used to be a pair
    // of mid-stream `Role::System` messages but now folds into the
    // leading system message via `external_system_suffix` so strict
    // chat templates (Qwen, Llama) accept it.
    let (context, compaction_suffix): (Vec<ChatMessage>, String) = if let Some(ref compactor) =
        ctx.compactor
    {
        match compactor
            .prepare_context(
                &arc_id,
                ctx.compaction_trigger_tokens,
                ctx.compaction_target_tokens,
            )
            .await
        {
            Ok(view) => crate::compaction::view_to_messages(&view),
            Err(e) => {
                tracing::warn!(arc = %arc_id, error = %e, "compactor.prepare_context failed; using empty context");
                (Vec::new(), String::new())
            }
        }
    } else if let Some(ref store) = ctx.arc_store {
        let messages = match store.load_entries(&arc_id).await {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.entry_type == athen_persistence::arcs::EntryType::Message)
                .filter_map(|e| {
                    let role = match e.source.as_str() {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "system" => Role::System,
                        "tool" => Role::Tool,
                        _ => return None,
                    };
                    Some(ChatMessage {
                        role,
                        content: MessageContent::Text(e.content),
                    })
                })
                .collect(),
            Err(_) => vec![],
        };
        (messages, String::new())
    } else {
        (vec![], String::new())
    };

    // `system_suffix` accumulates host-supplied volatile content that
    // used to ride as mid-stream `Role::System` messages. Strict chat
    // templates (Qwen, Llama) raise on non-leading system roles, so we
    // fold it into the leading system message via
    // `AgentBuilder::external_system_suffix`. Compaction output goes
    // first so the summary precedes memory recall.
    let mut system_suffix = compaction_suffix;

    // Auto-inject relevant memories into context.
    if let Some(ref memory) = ctx.memory {
        let mut all_items = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        if let Ok(items) = memory.recall(&message, 3).await {
            for item in items {
                if seen_ids.insert(item.id.clone()) {
                    all_items.push(item);
                }
            }
        }

        if !all_items.is_empty() {
            tracing::info!(
                count = all_items.len(),
                "Injecting relevant memories into dispatched task context"
            );
            let memory_text = all_items
                .iter()
                .map(|m| format!("- {}", m.content))
                .collect::<Vec<_>>()
                .join("\n");
            // Fold into the leading system message via
            // `external_system_suffix` instead of a mid-stream
            // `Role::System` push — strict chat templates (Qwen, Llama)
            // raise on non-leading system roles.
            system_suffix.push_str(&format!(
                "MEMORIES ALREADY LOADED FROM YOUR PERSISTENT MEMORY \
                 (treat these as authoritative — do not call memory_recall \
                 to re-fetch the same entities listed below; only call \
                 memory_recall if you need *additional* information not \
                 covered here):\n{memory_text}\n\n"
            ));
        }
    }

    // NOTE: deliberately NOT persisting a "user" message here. The
    // sense_router already wrote a "system" context message describing
    // the original trigger (email body, calendar reminder, etc.) into
    // this arc. A separate "user" turn would duplicate that.

    // Surface attachments tied to this sense event. Branches by
    // capability: vision-capable provider → images go into a Multimodal
    // user turn; PDF text sidecars get inlined as a System turn so the
    // agent can read them without a tool call. Other types are listed
    // by metadata (the agent can call fetch_attachment / read_attachment_full
    // for them on demand). No-op when there is no source_event, no
    // attachment store wired (CLI/tests), or no attachments for the event.
    let mut surfaced_images: Vec<athen_core::llm::ImageInput> = Vec::new();
    if let (Some(event_id), Some(astore)) = (task.source_event, ctx.attachment_store.as_ref()) {
        let router_guard = ctx.router.read().await;
        let supports_vision = router_guard.any_provider_supports_vision();
        let supports_documents = router_guard.any_provider_supports_documents();
        drop(router_guard);
        let surfacing =
            prepare_attachment_surfacing(event_id, astore, supports_vision, supports_documents)
                .await;
        if let Some(msg) = surfacing.system_message {
            tracing::info!(
                event_id = %event_id,
                images = surfacing.images.len(),
                surfaced_chars = msg.len(),
                context_messages_before = context.len(),
                "Surfacing attachments to dispatched executor"
            );
            // Fold into the leading system message instead of a
            // mid-stream `Role::System` push (Qwen/Llama Jinja).
            let surfaced_chars = msg.len();
            system_suffix.push_str(&msg);
            if !system_suffix.ends_with("\n\n") {
                system_suffix.push_str("\n\n");
            }
            tracing::info!(
                context_messages_after = context.len(),
                system_suffix_chars = system_suffix.len(),
                surfaced_chars,
                "Context after attachment surfacing"
            );
        }
        surfaced_images = surfacing.images;
    }

    // Wake-up autonomy directive — same shape as execute_approved_task.
    // The dispatch path is the *normal* wake-up route (SilentApprove /
    // NotifyAndProceed), so this is where the directive matters most.
    // Soft enforcement (the LLM must self-restrict); the hard layer
    // (tool/contact allowlists) wraps the registry below.
    if let Some(ref w) = ctx.wakeup {
        use athen_core::wakeup::AutonomyBand;
        let band_directive = match w.autonomy {
            AutonomyBand::Auto => {
                "AUTONOMY: auto. Run anything below `Critical` risk without \
                 prompting. Pause only on Critical actions and write a clear \
                 stop note to the arc."
            }
            AutonomyBand::SafeOnly => {
                "AUTONOMY: safe_only (default). Execute below-threshold \
                 actions without prompting. For anything Caution or above, \
                 stop and write a stop note to the arc — the user will see it \
                 next time they open Athen."
            }
            AutonomyBand::NotifyOnly => {
                "AUTONOMY: notify_only. You may read, summarize, and write to \
                 this arc. You MUST NOT send any outbound message (email, \
                 Telegram, etc.), call any contact, or trigger any external \
                 side effect. If the instruction implies an outbound action, \
                 stop, write what you would have sent into the arc, and exit."
            }
        };
        let header = format!(
            "[Wake-up trigger — id {}, fired by scheduler at {}]\n\
             You were not invoked by a live user; this run is unattended. \
             Output destination is governed by the instruction itself \
             (write to file / send / append). The user will review the arc \
             when they next open Athen.\n\n{band_directive}\n\n",
            w.id,
            chrono::Utc::now().to_rfc3339()
        );
        if !system_suffix.is_empty() && !system_suffix.ends_with("\n\n") {
            system_suffix.push_str("\n\n");
        }
        system_suffix.push_str(&header);
    }

    // Build the tool registry, mirroring execute_approved_task.
    let mut shell_registry = athen_agent::ShellToolRegistry::new()
        .await
        .with_spawned_processes(ctx.spawned_processes.clone())
        .with_web_search(Arc::clone(&ctx.web_search))
        .with_email_sender_opt(ctx.email_sender.clone());
    if let Some(ref store) = ctx.grant_store {
        let provider = Arc::new(crate::file_gate::ArcWritableProvider {
            arc_id: crate::file_gate::arc_uuid(&arc_id),
            store: store.clone(),
        });
        shell_registry = shell_registry.with_extra_writable(provider);
    }
    if let Some(ref router) = ctx.approval_router {
        shell_registry = shell_registry.with_toolbox_approval(Arc::new(
            crate::file_gate::RouterToolboxApprovalGate::new(
                Arc::clone(router),
                Some(arc_id.clone()),
            ),
        ));
        let gate: Arc<dyn athen_agent::EmailSendApprovalGate> =
            Arc::new(crate::email_gate::RouterEmailApprovalGate::new(
                Arc::clone(router),
                Some(arc_id.clone()),
            ));
        shell_registry = shell_registry.with_email_approval(gate);
    }
    let mut registry = crate::app_tools::AppToolRegistry::new(
        shell_registry,
        ctx.calendar_store.clone(),
        ctx.contact_store.clone(),
        ctx.memory.clone(),
    )
    .with_mcp(ctx.mcp.clone() as Arc<dyn athen_core::traits::mcp::McpClient>);
    if let Some(ref astore) = ctx.attachment_store {
        registry = registry.with_attachments(astore.clone());
    }
    if let Some(ref store) = ctx.grant_store {
        let mut gate = crate::file_gate::FileGate::new(
            arc_id.clone(),
            store.clone(),
            ctx.pending_grants.clone(),
            Some(ctx.app_handle.clone()),
        );
        if let Some(ref sink) = ctx.telegram_approval_sink {
            gate = gate.with_telegram_approval(sink.clone());
        }
        registry = registry.with_file_gate(Arc::new(gate));
    }

    // Pre-resolve wake-up restrictions; same shape as execute_approved_task.
    // See that function for the full design rationale.
    let wakeup_restrictions: Option<crate::wakeup_registry::WakeupSubagentRestrictions> =
        if let Some(ref w) = ctx.wakeup {
            let contact_store_dyn: Option<Arc<dyn athen_contacts::ContactStore>> = ctx
                .contact_store
                .as_ref()
                .map(|s| Arc::new(s.clone()) as Arc<dyn athen_contacts::ContactStore>);
            Some(
                crate::wakeup_registry::resolve_wakeup_restrictions(
                    w.tool_allowlist.clone(),
                    w.contact_allowlist.as_deref(),
                    w.autonomy,
                    contact_store_dyn.as_ref(),
                )
                .await,
            )
        } else {
            None
        };
    let subagent_inherit = ctx
        .wakeup
        .as_ref()
        .map(|w| w.inherit_restrictions)
        .unwrap_or(false);
    let subagent_restrictions = if subagent_inherit {
        wakeup_restrictions.clone()
    } else {
        None
    };

    let base_registry: Arc<dyn athen_core::traits::tool::ToolRegistry> = Arc::new(registry);
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> =
        if let Some(profile_store) = ctx.profile_store.clone() {
            if let Some(arc_store) = ctx.arc_store.clone() {
                let dctx = crate::delegation::DelegationContext {
                    profile_store,
                    identity_store: ctx.identity_store.clone(),
                    arc_store,
                    llm_router: Arc::clone(&ctx.router),
                    parent_arc_id: arc_id.clone(),
                    tool_doc_dir: ctx.tool_doc_dir.clone(),
                    app_handle: Some(ctx.app_handle.clone()),
                    wakeup_restrictions: subagent_restrictions,
                };
                Box::new(crate::delegation::DelegationToolRegistry::new(
                    base_registry,
                    dctx,
                ))
            } else {
                Box::new(crate::delegation::ArcRegistryAdapter(base_registry))
            }
        } else {
            Box::new(crate::delegation::ArcRegistryAdapter(base_registry))
        };

    // Wake-up authoring layer — adds `create_wakeup`. See execute_approved_task.
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> = match ctx.wakeup_store.clone() {
        Some(store) => {
            let wctx = crate::wakeup_tool::WakeupToolContext {
                wakeup_store: store,
                approval_router: ctx.approval_router.clone(),
                parent_arc_id: arc_id.clone(),
            };
            Box::new(crate::wakeup_tool::WakeupAuthoringRegistry::new(
                registry, wctx,
            ))
        }
        None => registry,
    };

    // Wake-up tool/contact allowlist — outermost. See execute_approved_task.
    let registry: Box<dyn athen_core::traits::tool::ToolRegistry> = match wakeup_restrictions {
        Some(restrictions) => Box::new(
            crate::wakeup_registry::WakeupRestrictedRegistry::new_with_resolved(
                registry,
                restrictions,
            ),
        ),
        None => registry,
    };

    let exec_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(&ctx.router)));
    let tool_log = new_tool_log();
    let auditor = TauriAuditor::new(
        ctx.app_handle.clone(),
        ctx.arc_store.clone(),
        arc_id.clone(),
        ctx.turn_id.clone(),
        tool_log.clone(),
    );
    let stream_tx = spawn_stream_forwarder(&ctx.app_handle, Some(arc_id.clone()));

    ctx.cancel_flag.store(false, Ordering::Relaxed);

    let context_snapshot = context.clone();

    let active_profile =
        resolve_active_profile(ctx.profile_store.as_ref(), ctx.arc_store.as_ref(), &arc_id).await;

    let identity_profile_id = active_profile
        .as_ref()
        .map(|p| p.profile.id.clone())
        .unwrap_or_else(|| athen_core::agent_profile::AgentProfile::DEFAULT_ID.to_string());
    let identity_block = crate::identity_render::render_identity_block(
        ctx.identity_store.as_ref(),
        &identity_profile_id,
    )
    .await;

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(registry)
        .auditor(Box::new(auditor))
        .max_steps(50)
        .timeout(Duration::from_secs(300))
        .context_messages(context)
        .stream_sender(stream_tx)
        .cancel_flag(ctx.cancel_flag.clone())
        .external_system_suffix(Some(system_suffix))
        .autonomous_mode(true)
        .identity_block(identity_block)
        .default_temperature(ctx.sampling_temperature);
    if let Some(p) = ctx.tool_doc_dir.clone() {
        builder = builder.tool_doc_dir(p);
    }
    if let Some(profile) = active_profile {
        builder = builder.active_profile(profile);
    }
    builder = builder.toolbox_info(athen_agent::toolbox::ToolboxPromptInfo::load().await);
    if !surfaced_images.is_empty() {
        builder = builder.initial_user_images(surfaced_images);
    }
    let executor = builder.build().map_err(|e| {
        let raw = e.to_string();
        tracing::error!("AgentBuilder failed (dispatched): {raw}");
        format_user_error(&raw)
    })?;

    // Build the task we hand to the executor. Reuse the source_event,
    // domain, priority, and risk fields the coordinator already filled
    // in — we just need a fresh inner id for executor bookkeeping.
    let exec_task = Task {
        id: Uuid::new_v4(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        source_event: task.source_event,
        domain: task.domain.clone(),
        description: message.clone(),
        priority: task.priority,
        status: TaskStatus::InProgress,
        risk_score: task.risk_score.clone(),
        risk_budget: task.risk_budget,
        risk_used: task.risk_used,
        assigned_agent: None,
        steps: vec![],
        deadline: None,
    };

    let result = match executor.execute(exec_task).await {
        Ok(r) => r,
        Err(e) => {
            let _ = ctx.coordinator.complete_task(coord_task_id).await;
            let raw = e.to_string();
            tracing::error!("Agent execution failed for dispatched task: {raw}");
            let msg = format_user_error(&raw);

            if let Some(ref store) = ctx.arc_store {
                if let Err(e) = store
                    .add_entry(
                        &arc_id,
                        athen_persistence::arcs::EntryType::Message,
                        "assistant",
                        &msg,
                        None,
                        Some(&ctx.turn_id),
                    )
                    .await
                {
                    warn!("Failed to persist dispatched-task error reply: {e}");
                }
                if let Err(e) = store.touch_arc(&arc_id).await {
                    warn!("Failed to touch arc on error path: {e}");
                }
            }

            return Ok(Some(ApprovedTaskOutcome {
                content: msg,
                success: false,
                domain: task.domain.clone(),
                message,
                context_snapshot,
                tool_log,
            }));
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
            "Task cancelled.".to_string()
        } else {
            format!(
                "Ran out of steps ({} used) before finishing.",
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

    // Persist the assistant response.
    if let Some(ref store) = ctx.arc_store {
        if let Err(e) = store
            .add_entry(
                &arc_id,
                athen_persistence::arcs::EntryType::Message,
                "assistant",
                &content,
                None,
                Some(&ctx.turn_id),
            )
            .await
        {
            warn!("Failed to persist dispatched-task assistant entry: {e}");
        }
        if let Err(e) = store.touch_arc(&arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }

    // Reinforce memories that were actually used.
    if let Some(ref memory) = ctx.memory {
        reinforce_used_memories(memory, &context_snapshot, &content).await;
    }

    // Auto-remember with the LLM judge.
    if let Some(ref memory) = ctx.memory {
        let router = SharedRouter(Arc::clone(&ctx.router));
        let arc_id_clone = arc_id.clone();
        let msg_clone = message.clone();
        let content_clone = content.clone();
        let memory_clone = Arc::clone(memory);
        tokio::spawn(async move {
            match judge_worth_remembering(
                &router,
                memory_clone.as_ref(),
                &msg_clone,
                &content_clone,
            )
            .await
            {
                Some(summary) => {
                    tracing::info!("Memory judge: worth remembering (dispatched task)");
                    let item = athen_core::traits::memory::MemoryItem {
                        id: uuid::Uuid::new_v4().to_string(),
                        content: summary,
                        metadata: serde_json::json!({
                            "source": "conversation",
                            "arc_id": arc_id_clone,
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                        }),
                    };
                    if let Err(e) = memory_clone.remember(item).await {
                        tracing::warn!("Failed to remember interaction: {e}");
                    }
                }
                None => {
                    tracing::debug!("Memory judge: not worth remembering (dispatched task)");
                }
            }
        });
    }

    let _ = ctx.coordinator.complete_task(coord_task_id).await;

    let _ = ctx
        .app_handle
        .emit("arc-updated", serde_json::json!({ "arc_id": arc_id }));

    // Completion ping. Only fires on success — early-return error branches
    // above already surfaced their own assistant entry into the arc, and
    // we don't want a "Athen finished" toast for a failed run.
    if result.success {
        if let Some(ref notifier) = ctx.notifier {
            let arc_name = if let Some(ref store) = ctx.arc_store {
                match store.get_arc(&arc_id).await {
                    Ok(Some(meta)) => meta.name,
                    _ => arc_id.clone(),
                }
            } else {
                arc_id.clone()
            };
            let body_notif = if content.is_empty() {
                "Task completed.".to_string()
            } else if content.chars().count() > 140 {
                let cap = content.floor_char_boundary(140);
                format!("{}...", &content[..cap])
            } else {
                content.clone()
            };
            // Carry the full content for chat-style channels (Telegram).
            // The 140-char `body` above is for the InApp toast preview;
            // Telegram should show the entire reply.
            let body_long = if content.chars().count() > 140 && !content.is_empty() {
                Some(content.clone())
            } else {
                None
            };
            let notification = athen_core::notification::Notification {
                id: Uuid::new_v4(),
                urgency: athen_core::notification::NotificationUrgency::Low,
                title: format!("Athen finished: {arc_name}"),
                body: body_notif,
                origin: athen_core::notification::NotificationOrigin::Agent,
                arc_id: Some(arc_id.clone()),
                task_id: None,
                created_at: chrono::Utc::now(),
                requires_response: false,
                skip_humanize: true,
                body_long,
            };
            notifier.notify(notification).await;
        }
    }

    Ok(Some(ApprovedTaskOutcome {
        content,
        success: result.success,
        domain: task.domain.clone(),
        message,
        context_snapshot,
        tool_log,
    }))
}

/// Cancel the currently running agent task.
///
/// Sets the shared cancellation flag to `true`, which the executor checks
/// at the top of each loop iteration and between tool calls. The executor
/// will return a "cancelled" result on its next check.
#[tauri::command]
pub async fn cancel_task(state: State<'_, AppState>) -> std::result::Result<(), String> {
    state.cancel_flag.store(true, Ordering::Relaxed);
    Ok(())
}

/// Resolve a pending [`ApprovalQuestion`] from the in-app UI.
///
/// Used by the new approval router flow: when the frontend renders an
/// approval prompt it received via the `approval-question` event, the
/// user's tap is forwarded here and the matching parked oneshot is
/// completed. Returns `false` if the question id is unknown (e.g. it
/// was already answered through another channel).
#[tauri::command]
pub async fn submit_approval(
    question_id: String,
    choice_key: String,
    state: State<'_, AppState>,
) -> std::result::Result<bool, String> {
    use athen_core::approval::ApprovalAnswer;

    let q_id = Uuid::parse_str(&question_id).map_err(|e| format!("Invalid question_id: {e}"))?;
    let Some(sink) = state.inapp_approval_sink.clone() else {
        return Ok(false);
    };
    let resolved = sink
        .resolve(ApprovalAnswer {
            question_id: q_id,
            choice_key,
        })
        .await;
    Ok(resolved)
}

/// Return basic status information.
#[tauri::command]
pub async fn get_status(state: State<'_, AppState>) -> std::result::Result<StatusResponse, String> {
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
pub async fn new_arc(state: State<'_, AppState>) -> std::result::Result<String, String> {
    *state.history.lock().await = Vec::new();
    let new_id = chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string();
    *state.active_arc_id.lock().await = new_id.clone();

    if let Some(ref store) = state.arc_store {
        if let Err(e) = store
            .create_arc(&new_id, "New Arc", arcs::ArcSource::UserInput)
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
        return Ok(entries.into_iter().map(Into::into).collect());
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
                turn_id: None,
            })
        })
        .collect())
}

/// Load entries for a specific arc by id. Used by the frontend to fetch
/// a delegation sub-arc's tool calls when rendering the inline expandable
/// view under the parent's `delegate_to_agent` result.
#[tauri::command]
pub async fn get_arc_entries(
    arc_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<ArcEntryResponse>, String> {
    if let Some(ref store) = state.arc_store {
        let entries = store
            .load_entries(&arc_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(entries.into_iter().map(Into::into).collect())
    } else {
        Ok(Vec::new())
    }
}

/// Outcome of a manual compaction request, shaped for the frontend.
/// Mirrors `athen_core::traits::compaction::CompactionOutcome` but in a
/// JSON-friendly form Tauri can serialize without leaking the trait
/// type into the public API surface.
#[derive(Serialize)]
pub struct CompactArcResponse {
    pub compacted: bool,
    pub summarized_through_entry_id: Option<i64>,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

/// User-triggered compaction. Forces a compaction pass on `arc_id`
/// regardless of the current budget — the trigger is the user's
/// intent, not the size estimate. Returns the outcome so the UI can
/// surface "compacted N→M tokens" feedback.
///
/// No-op (returns `compacted: false`) when the arc has too few entries
/// since the last summary to be worth collapsing — the per-impl floor
/// still applies. That prevents a click on a near-empty arc from
/// burning an LLM call to summarize one turn into nothing.
#[tauri::command]
pub async fn compact_arc(
    arc_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<CompactArcResponse, String> {
    let Some(ref compactor) = state.compactor else {
        return Err("Compactor not wired (no arc store).".into());
    };
    // target_tokens = 0 is the trait's "force" signal — see
    // `ArcCompactor::compact` docs.
    let outcome = compactor
        .compact(&arc_id, 0)
        .await
        .map_err(|e| e.to_string())?;
    Ok(CompactArcResponse {
        compacted: outcome.compacted,
        summarized_through_entry_id: outcome.summarized_through_entry_id,
        tokens_before: outcome.tokens_before,
        tokens_after: outcome.tokens_after,
    })
}

/// List root arcs with metadata for the sidebar. Delegation sub-arcs
/// (those with a `parent_arc_id`) are hidden — their content is rendered
/// inline under the parent's `delegate_to_agent` tool call instead.
#[tauri::command]
pub async fn list_arcs(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<arcs::ArcMeta>, String> {
    if let Some(ref store) = state.arc_store {
        store.list_root_arcs().await.map_err(|e| e.to_string())
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
                .map(Into::into)
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

        return Ok(entries.into_iter().map(Into::into).collect());
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
        store.delete_arc(&arc_id).await.map_err(|e| e.to_string())?;
    }

    // If deleting the active arc, switch to next or create new.
    let current = state.active_arc_id.lock().await.clone();
    if arc_id == current {
        if let Some(ref store) = state.arc_store {
            let all_arcs = store.list_root_arcs().await.map_err(|e| e.to_string())?;
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
        let new_id = chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string();
        if let Some(ref store) = state.arc_store {
            let _ = store
                .create_arc(&new_id, "New Arc", arcs::ArcSource::UserInput)
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
pub async fn get_current_arc(state: State<'_, AppState>) -> std::result::Result<String, String> {
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
    let new_id = chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string();
    if let Some(ref store) = state.arc_store {
        store
            .create_arc_with_parent(&new_id, &name, arcs::ArcSource::UserInput, &parent_arc_id)
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
// Agent profile commands
// ---------------------------------------------------------------------------

/// List every `AgentProfile` known to the system, built-ins first.
///
/// The seeded `default` profile always appears, followed by any
/// user-authored profiles. UI uses this to populate the per-arc profile
/// picker.
#[tauri::command]
pub async fn list_agent_profiles(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<athen_core::agent_profile::AgentProfile>, String> {
    use athen_core::traits::profile::ProfileStore;
    let Some(store) = state.profile_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.list_profiles().await.map_err(|e| e.to_string())
}

/// Set the agent profile this arc runs under.
///
/// Pass `None` (or omit the field) to clear the override and fall back to
/// the seeded default profile. The change is durable — subsequent tasks in
/// the arc run under the new profile.
#[tauri::command]
pub async fn set_arc_profile(
    arc_id: String,
    profile_id: Option<String>,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let Some(arc_store) = state.arc_store.as_ref() else {
        return Err("Arc store not available".into());
    };
    arc_store
        .set_active_profile_id(&arc_id, profile_id.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Inputs the manager UI sends when creating or updating a user-authored
/// profile. Mirrors `AgentProfile` minus the server-managed fields
/// (`builtin`, `created_at`, `updated_at`) and the unused-yet
/// `persona_template_ids`. Expertise is structured so the UI can drive it
/// with checkboxes/chip pickers without duplicating the enum spelling.
#[derive(serde::Deserialize, Debug)]
pub struct AgentProfileInput {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub custom_persona_addendum: Option<String>,
    #[serde(default)]
    pub tool_selection: Option<athen_core::agent_profile::ToolSelection>,
    #[serde(default)]
    pub expertise: athen_core::agent_profile::ExpertiseDeclaration,
    #[serde(default)]
    pub model_profile_hint: Option<String>,
}

fn input_to_profile(
    input: AgentProfileInput,
    created_at: chrono::DateTime<chrono::Utc>,
) -> athen_core::agent_profile::AgentProfile {
    use athen_core::agent_profile::{AgentProfile, ToolSelection};
    let now = chrono::Utc::now();
    AgentProfile {
        id: input.id,
        display_name: input.display_name,
        description: input.description,
        persona_template_ids: vec![],
        custom_persona_addendum: input.custom_persona_addendum,
        tool_selection: input.tool_selection.unwrap_or(ToolSelection::All),
        expertise: input.expertise,
        model_profile_hint: input.model_profile_hint,
        builtin: false,
        created_at,
        updated_at: now,
    }
}

/// Create a new user-authored profile.
///
/// Refuses to create a profile whose id collides with an existing one
/// (built-in or user). Built-in id reuse is the most common collision —
/// the UI's "Clone" flow appends a suffix to avoid it.
#[tauri::command]
pub async fn create_agent_profile(
    input: AgentProfileInput,
    state: State<'_, AppState>,
) -> std::result::Result<athen_core::agent_profile::AgentProfile, String> {
    use athen_core::traits::profile::ProfileStore;
    let Some(store) = state.profile_store.as_ref() else {
        return Err("Profile store not available".into());
    };
    let id = input.id.trim().to_string();
    if id.is_empty() {
        return Err("Profile id cannot be empty".into());
    }
    if store
        .get_profile(&id)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("Profile id '{id}' is already in use"));
    }
    let profile = input_to_profile(AgentProfileInput { id, ..input }, chrono::Utc::now());
    store
        .save_profile(&profile)
        .await
        .map_err(|e| e.to_string())?;
    Ok(profile)
}

/// Update an existing profile in place.
///
/// Both user-authored and built-in profiles can be edited. The store
/// preserves the existing row's `builtin` flag — built-ins stay marked
/// as built-in even after editing, so the seeder still treats the id as
/// already-seeded and the UI keeps its badge.
#[tauri::command]
pub async fn update_agent_profile(
    input: AgentProfileInput,
    state: State<'_, AppState>,
) -> std::result::Result<athen_core::agent_profile::AgentProfile, String> {
    use athen_core::traits::profile::ProfileStore;
    let Some(store) = state.profile_store.as_ref() else {
        return Err("Profile store not available".into());
    };
    let existing = store
        .get_profile(&input.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Profile '{}' not found", input.id))?;
    let profile = input_to_profile(input, existing.created_at);
    store
        .save_profile(&profile)
        .await
        .map_err(|e| e.to_string())?;
    // Re-read so the response reflects the store's authoritative `builtin`
    // flag and the freshly-stamped `updated_at`.
    let loaded = store
        .get_profile(&profile.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Profile '{}' missing after save", profile.id))?;
    Ok(loaded)
}

/// Delete a user-authored profile.
///
/// Refuses to delete built-ins. Any arcs referencing the deleted profile
/// will fall back to the seeded default at next resolution (the
/// `get_or_default` lookup tolerates dangling ids).
#[tauri::command]
pub async fn delete_agent_profile(
    profile_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    use athen_core::traits::profile::ProfileStore;
    let Some(store) = state.profile_store.as_ref() else {
        return Err("Profile store not available".into());
    };
    store
        .delete_profile(&profile_id)
        .await
        .map_err(|e| e.to_string())
}

/// Rewrite a built-in profile back to its canonical seeded values.
///
/// Only valid for ids in the canonical built-in list. User-authored
/// profiles return an error — they have no "default" to restore to.
#[tauri::command]
pub async fn restore_agent_profile(
    profile_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<athen_core::agent_profile::AgentProfile, String> {
    let Some(store) = state.profile_store.as_ref() else {
        return Err("Profile store not available".into());
    };
    store
        .restore_builtin(&profile_id)
        .await
        .map_err(|e| e.to_string())
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
        store
            .list_events(&start, &end)
            .await
            .map_err(|e| e.to_string())
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
        store
            .create_event(&event)
            .await
            .map_err(|e| e.to_string())?;
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
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Invalid notification ID: {e}"))?;

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
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Invalid notification ID: {e}"))?;
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
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("Invalid notification ID: {e}"))?;
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
    memory
        .update(&id, &content)
        .await
        .map_err(|e| e.to_string())
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
        .map(
            |(from_id, from_name, relation, to_id, to_name)| RelationInfo {
                from_id: from_id.to_string(),
                from_name,
                relation,
                to_id: to_id.to_string(),
                to_name,
            },
        )
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
    let entity_id = Uuid::parse_str(&id).map_err(|e| format!("Invalid entity ID: {e}"))?;
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
    let entity_id = Uuid::parse_str(&id).map_err(|e| format!("Invalid entity ID: {e}"))?;
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
    let from = Uuid::parse_str(&from_id).map_err(|e| format!("Invalid from entity ID: {e}"))?;
    let to = Uuid::parse_str(&to_id).map_err(|e| format!("Invalid to entity ID: {e}"))?;
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
    let grants = store.list_arc(arc_uuid).await.map_err(|e| e.to_string())?;
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
    let grants = store.list_global().await.map_err(|e| e.to_string())?;
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
    store
        .revoke_arc_by_id(id)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
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
    store
        .revoke_global_by_id(id)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub version: Option<String>,
    pub current_version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
    /// "appimage" → in-app updater can swap the binary.
    /// "system"   → packaged via rpm/deb/aur/dmg; user must update through their package manager.
    pub installer_kind: String,
    /// Release page URL — surfaced when `installer_kind == "system"` so the UI can
    /// link the user to the right download instead of attempting an in-place update.
    pub release_url: Option<String>,
}

/// Linux AppImage runtimes export `APPIMAGE` pointing at the .AppImage path.
/// macOS/Windows installs are always self-updatable via tauri-plugin-updater.
fn detect_installer_kind() -> &'static str {
    if cfg!(target_os = "linux") {
        if std::env::var_os("APPIMAGE").is_some() {
            "appimage"
        } else {
            "system"
        }
    } else {
        "appimage"
    }
}

fn release_url_for(version: &str) -> String {
    format!("https://github.com/albiol2004/Athen/releases/tag/v{version}")
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> std::result::Result<UpdateInfo, String> {
    use tauri_plugin_updater::UpdaterExt;

    let current_version = app.package_info().version.to_string();
    let installer_kind = detect_installer_kind().to_string();
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateInfo {
            available: true,
            version: Some(update.version.clone()),
            current_version,
            notes: update.body.clone(),
            date: update.date.map(|d| d.to_string()),
            release_url: Some(release_url_for(&update.version)),
            installer_kind,
        }),
        Ok(None) => Ok(UpdateInfo {
            available: false,
            version: None,
            current_version,
            notes: None,
            date: None,
            release_url: None,
            installer_kind,
        }),
        Err(e) => Err(format!("update check failed: {}", e)),
    }
}

/// Open a URL in the user's default browser. Used by the update banner when the
/// install is system-managed (rpm/deb/aur) and we can't self-update — we send
/// the user to the GitHub release page instead.
#[tauri::command]
pub async fn open_external_url(url: String) -> std::result::Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http(s) URLs are allowed".to_string());
    }
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .creation_flags(0x0800_0000)
            .args(["/C", "start", "", &url])
            .spawn()
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "unsupported platform",
    ));
    result.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn install_update(
    app: AppHandle,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    if detect_installer_kind() == "system" {
        return Err(
            "This install is managed by your system package manager (rpm/deb/aur). \
             Update through it (e.g. `sudo dnf upgrade athen` or `yay -Syu athen-bin`) \
             or download the new release from GitHub."
                .to_string(),
        );
    }

    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("update check failed: {}", e))?
        .ok_or_else(|| "no update available".to_string())?;

    // Force-kill every `shell_spawn`'d watcher before the installer
    // starts. The agent's prompt funnels "monitor X" into shell_spawn,
    // and on Windows the model has been observed to chain `cmd /C nu …`
    // when raw cmd syntax fails — that nu grandchild then locks the
    // bundled sidecar. Tracking is in-memory anyway, so anything we
    // don't kill becomes an unmanageable orphan after restart.
    let killed = athen_agent::kill_all_spawned(&state.spawned_processes).await;
    if killed > 0 {
        tracing::info!(count = killed, "killed spawned processes before update");
    }

    // Close the shell drain gate and wait for in-flight commands to
    // finish. On Windows the installer can't overwrite the bundled
    // `nu.exe` sidecar while it's still running — symptom is the
    // "Error opening nu.exe to write" failure users hit on the v0.1.3
    // → v0.1.4 update. After this returns the gate stays closed; new
    // shell calls fail fast until the app restarts.
    let drained = athen_shell::drain::drain_for_update(Duration::from_secs(10)).await;
    if !drained {
        tracing::warn!("Shell drain timed out before update; install may still hit a sidecar lock");
    }

    // Retry install on transient failures. On Windows an antivirus
    // scanner can hold a brief handle on the sidecar even after the
    // child exits, so a one-or-two second pause often clears it.
    let mut last_err: Option<String> = None;
    for attempt in 0..3 {
        match update
            .download_and_install(|_chunk_len, _content_len| {}, || {})
            .await
        {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                let msg = format!("download/install failed: {}", e);
                tracing::warn!("update attempt {} failed: {}", attempt + 1, msg);
                last_err = Some(msg);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    if let Some(err) = last_err {
        return Err(err);
    }

    // Restart so the freshly installed binary takes over. On Windows the
    // installer already replaces the running .exe and `restart()` will
    // re-launch from the new path.
    app.restart();
}

/// Frontend-friendly view of an [`athen_agent::InstalledPackage`].
/// `runtime` is serialized as `"python"` / `"node"` so the JS side
/// doesn't need to know the Rust enum representation.
#[derive(Serialize)]
pub struct ToolboxPackageView {
    pub runtime: String,
    pub package: String,
    pub version_spec: Option<String>,
    pub installed_version: Option<String>,
    pub reason: String,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub runtime_version: Option<String>,
}

impl From<athen_agent::InstalledPackage> for ToolboxPackageView {
    fn from(p: athen_agent::InstalledPackage) -> Self {
        Self {
            runtime: p.runtime.as_str().to_string(),
            package: p.package,
            version_spec: p.version_spec,
            installed_version: p.installed_version,
            reason: p.reason,
            installed_at: p.installed_at,
            runtime_version: p.runtime_version,
        }
    }
}

#[tauri::command]
pub async fn list_toolbox_packages() -> std::result::Result<Vec<ToolboxPackageView>, String> {
    let manifest = athen_agent::toolbox::load_manifest().await;
    Ok(manifest.installs.into_iter().map(Into::into).collect())
}

#[tauri::command]
pub async fn clear_toolbox() -> std::result::Result<(), String> {
    athen_agent::toolbox::clear_toolbox()
        .await
        .map_err(|e| e.to_string())
}

// ─── Portable runtime install (onboarding wizard) ────────────────────
//
// The wizard probes for system Python / Node and, if either is missing,
// offers to install a portable copy under
// `<athen_data_dir>/toolbox/runtimes/`. These commands back that UI:
// `get_runtime_status` for the snapshot, `install_runtime` for the
// download. Progress is streamed back as `runtime-install-progress`
// events keyed by runtime kind so the frontend can show a real bar.

#[tauri::command]
pub async fn get_runtime_status(
) -> std::result::Result<athen_agent::runtimes::RuntimesStatus, String> {
    Ok(athen_agent::runtimes::status().await)
}

#[derive(Serialize, Clone)]
struct RuntimeInstallEvent {
    kind: String,
    progress: athen_agent::runtimes::InstallProgress,
}

#[tauri::command]
pub async fn install_runtime(
    app: AppHandle,
    kind: String,
) -> std::result::Result<athen_agent::runtimes::PortableRuntimeRecord, String> {
    let parsed = athen_agent::runtimes::RuntimeKind::parse(&kind)
        .ok_or_else(|| format!("unknown runtime kind '{kind}'"))?;
    let kind_label = parsed.as_str().to_string();
    let app_for_cb = app.clone();
    let label_for_cb = kind_label.clone();
    let progress: athen_agent::runtimes::ProgressCb = Arc::new(move |p| {
        let _ = app_for_cb.emit(
            "runtime-install-progress",
            RuntimeInstallEvent {
                kind: label_for_cb.clone(),
                progress: p,
            },
        );
    });
    athen_agent::runtimes::install_runtime(parsed, progress)
        .await
        .map_err(|e| e.to_string())
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

// ---------------------------------------------------------------------------
// Identity store commands
// ---------------------------------------------------------------------------
//
// CRUD over the user-editable identity store: categories (groupings like
// `personality`, `rules`, plus user-invented ones) and entries (the actual
// statements). Always returns the full row so the UI can update its local
// state without an immediate re-list.

/// List every identity category, ordered by `sort_order` ascending.
#[tauri::command]
pub async fn list_identity_categories(
    state: State<'_, AppState>,
) -> std::result::Result<Vec<athen_core::identity::IdentityCategory>, String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.list_categories().await.map_err(|e| e.to_string())
}

/// Input shape for upserting a category. The `is_seed` flag is preserved
/// from the existing row when present, so a user-edited seed stays flagged
/// as a seed.
#[derive(serde::Deserialize, Debug)]
pub struct IdentityCategoryInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_applies_to: Vec<athen_core::identity::ProfileTag>,
    pub sort_order: u32,
}

/// Insert or update a category by `name`.
///
/// Names are user-controlled; trim happens before validation so a name of
/// just whitespace is rejected. The seed flag of an existing row survives —
/// renaming a seed category still shows the seed badge.
#[tauri::command]
pub async fn upsert_identity_category(
    input: IdentityCategoryInput,
    state: State<'_, AppState>,
) -> std::result::Result<athen_core::identity::IdentityCategory, String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Err("Identity store not available".into());
    };
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err("Category name cannot be empty".into());
    }
    let existing_seed = store
        .get_category(&name)
        .await
        .map_err(|e| e.to_string())?
        .map(|c| c.is_seed)
        .unwrap_or(false);
    let category = athen_core::identity::IdentityCategory {
        name: name.clone(),
        description: input.description,
        default_applies_to: input.default_applies_to,
        sort_order: input.sort_order,
        is_seed: existing_seed,
    };
    store
        .upsert_category(&category)
        .await
        .map_err(|e| e.to_string())?;
    Ok(category)
}

/// Delete a category and cascade-delete its entries.
///
/// Allowed for both seed and user categories — the user can clear any
/// category they want. The UI surfaces a confirm before calling this when
/// the category has entries or is a seed.
#[tauri::command]
pub async fn delete_identity_category(
    name: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Err("Identity store not available".into());
    };
    store
        .delete_category(&name)
        .await
        .map_err(|e| e.to_string())
}

/// List entries, optionally scoped to a single category. With no filter,
/// returns every entry ordered by `(category sort_order, updated_at DESC)`.
#[tauri::command]
pub async fn list_identity_entries(
    category: Option<String>,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<athen_core::identity::IdentityEntry>, String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Ok(Vec::new());
    };
    store
        .list_entries(category.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Input shape for upserting an entry. `id` is `None` on create.
#[derive(serde::Deserialize, Debug)]
pub struct IdentityEntryInput {
    #[serde(default)]
    pub id: Option<String>,
    pub category: String,
    pub body: String,
    #[serde(default)]
    pub applies_to: Vec<athen_core::identity::ProfileTag>,
    #[serde(default)]
    pub pinned: bool,
}

/// Insert or update an entry. Returns the persisted row including the
/// store-stamped `updated_at`.
///
/// When `id` is omitted, a fresh UUID is generated. Empty applies_to is
/// allowed but will scope the entry to no profiles — the UI shows a warning.
#[tauri::command]
pub async fn upsert_identity_entry(
    input: IdentityEntryInput,
    state: State<'_, AppState>,
) -> std::result::Result<athen_core::identity::IdentityEntry, String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Err("Identity store not available".into());
    };
    let id = match input.id {
        Some(s) => uuid::Uuid::parse_str(&s).map_err(|e| format!("Invalid entry id: {e}"))?,
        None => uuid::Uuid::new_v4(),
    };
    let now = chrono::Utc::now();
    let entry = athen_core::identity::IdentityEntry {
        id,
        category: input.category,
        body: input.body,
        applies_to: input.applies_to,
        pinned: input.pinned,
        // User-driven path always lands as `false`; only `identity_add`
        // (the agent tool) sets this true.
        proposed_by_agent: false,
        created_at: now,
        updated_at: now,
    };
    store
        .upsert_entry(&entry)
        .await
        .map_err(|e| e.to_string())?;
    let loaded = store
        .get_entry(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Entry missing after save".to_string())?;
    Ok(loaded)
}

/// Delete an entry by id.
#[tauri::command]
pub async fn delete_identity_entry(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Err("Identity store not available".into());
    };
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid entry id: {e}"))?;
    store.delete_entry(uuid).await.map_err(|e| e.to_string())
}

/// Dismiss an agent-proposed identity entry. Same delete path as
/// `delete_identity_entry`; the distinct command name lets the UI surface a
/// "remove suggestion" action and keeps the audit log readable.
#[tauri::command]
pub async fn dismiss_identity_entry(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    use athen_core::traits::identity::IdentityStore;
    let Some(store) = state.identity_store.as_ref() else {
        return Err("Identity store not available".into());
    };
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid entry id: {e}"))?;
    store.delete_entry(uuid).await.map_err(|e| e.to_string())
}

/// Wire shape for an attachment thumbnail returned to the frontend.
/// Image rows ship `data_url` populated with a `data:<mime>;base64,...`
/// payload so the UI can render them inline without a second round-trip;
/// non-image rows ship `data_url: None` and the UI shows a name + icon
/// chip. After TTL purge, even image rows go to `data_url: None` —
/// `purged: true` lets the UI gray them out instead of trying to fetch.
#[derive(Serialize)]
pub struct AttachmentThumbnail {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub purged: bool,
    pub data_url: Option<String>,
}

/// List attachments tied to a synthesized `event_id` and shape them
/// into thumbnail wire records for the frontend. Used by the chat
/// renderer when a user-message arc entry's metadata carries
/// `attachment_event_id` — the bubble can then render images inline
/// and file chips for non-image MIMEs.
///
/// Image bytes are inlined (≤ ~2MB per file in practice; AttachmentPolicy
/// already caps inbound size). For the rare oversize image, we fall
/// back to no data_url and the UI shows a chip so the user still sees
/// it existed.
#[tauri::command]
pub async fn list_attachments_for_event(
    event_id: String,
    state: State<'_, AppState>,
) -> std::result::Result<Vec<AttachmentThumbnail>, String> {
    use base64::Engine;

    const MAX_INLINE_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

    let Some(store) = state.attachment_store() else {
        return Ok(Vec::new());
    };
    let event_uuid = Uuid::parse_str(&event_id).map_err(|e| format!("Invalid event id: {e}"))?;
    let atts = store
        .list_for_event(event_uuid)
        .await
        .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(atts.len());
    for att in atts {
        let is_image = att.mime_type.to_ascii_lowercase().starts_with("image/");
        let purged = att.is_purged();
        let data_url = match att.local_path.as_ref() {
            Some(path) if is_image && !purged && att.size_bytes <= MAX_INLINE_IMAGE_BYTES => {
                match tokio::fs::read(path).await {
                    Ok(bytes) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        Some(format!("data:{};base64,{}", att.mime_type, b64))
                    }
                    Err(e) => {
                        tracing::warn!(
                            attachment_id = %att.id,
                            error = %e,
                            "Failed to read attachment bytes for thumbnail"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        out.push(AttachmentThumbnail {
            id: att.id.0.to_string(),
            name: att.name,
            mime_type: att.mime_type,
            size_bytes: att.size_bytes,
            purged,
            data_url,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod attachment_surfacing_tests {
    use super::prepare_attachment_surfacing;
    use athen_core::event::{Attachment, AttachmentSource};
    use athen_persistence::Database;
    use std::path::PathBuf;
    use uuid::Uuid;

    async fn fresh_store() -> athen_persistence::attachments::AttachmentStore {
        let tmp = std::env::temp_dir().join(format!("athen_att_surf_{}.db", Uuid::new_v4()));
        let db = Database::new(&tmp).await.unwrap();
        db.attachment_store()
    }

    #[tokio::test]
    async fn no_attachments_returns_empty_message() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();
        let result = prepare_attachment_surfacing(event_id, &store, true, true).await;
        assert!(result.images.is_empty());
        assert!(result.system_message.is_none());
    }

    #[tokio::test]
    async fn pdf_with_sidecar_inlines_full_text() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let dir = std::env::temp_dir().join(format!("athen_att_pdf_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_path = dir.join("invoice.pdf");
        std::fs::write(&pdf_path, b"PDF-bytes-not-actually-used-by-test").unwrap();
        let txt_path = dir.join("invoice.pdf.txt");
        std::fs::write(&txt_path, "Hello PDF world").unwrap();

        let mut att = Attachment::new(
            "invoice.pdf",
            "application/pdf",
            33,
            Some(pdf_path.clone()),
            Some(AttachmentSource::Email {
                account_id: "primary".into(),
                mailbox: "INBOX".into(),
                uid_validity: 1,
                uid: 1,
                part_path: "1".into(),
            }),
        );
        att.extracted_text_path = Some(txt_path.clone());
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, false, false).await;
        assert!(result.images.is_empty());
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("invoice.pdf"));
        assert!(msg.contains("Hello PDF world"));
        assert!(msg.contains("PDF text inlined in full"));
        assert!(!msg.contains("call read_attachment_full"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pdf_with_long_sidecar_truncates_and_dangles_tool() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let dir = std::env::temp_dir().join(format!("athen_att_pdf_long_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_path = dir.join("big.pdf");
        std::fs::write(&pdf_path, b"PDF").unwrap();
        let txt_path = dir.join("big.pdf.txt");
        let long = "x".repeat(20_000);
        std::fs::write(&txt_path, &long).unwrap();

        let mut att = Attachment::new(
            "big.pdf",
            "application/pdf",
            3,
            Some(pdf_path),
            Some(AttachmentSource::Email {
                account_id: "primary".into(),
                mailbox: "INBOX".into(),
                uid_validity: 1,
                uid: 1,
                part_path: "1".into(),
            }),
        );
        att.extracted_text_path = Some(txt_path);
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, false, false).await;
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("call read_attachment_full"));
        assert!(msg.contains("of 20000 chars"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn image_with_vision_inlines_base64_bytes() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let dir = std::env::temp_dir().join(format!("athen_att_img_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let img_path = dir.join("photo.png");
        // Bytes don't need to be a valid PNG — base64 just encodes them.
        let img_bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        std::fs::write(&img_path, &img_bytes).unwrap();

        let att = Attachment::new(
            "photo.png",
            "image/png",
            img_bytes.len() as u64,
            Some(img_path.clone()),
            Some(AttachmentSource::Telegram {
                chat_id: 1,
                message_id: 1,
                file_id: "f".into(),
            }),
        );
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, true, false).await;
        assert_eq!(result.images.len(), 1);
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("photo.png"));
        assert!(msg.contains("inlined as multimodal image"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn image_without_vision_falls_back_to_metadata() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let dir = std::env::temp_dir().join(format!("athen_att_img_nv_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let img_path = dir.join("photo.jpg");
        std::fs::write(&img_path, [0u8; 8]).unwrap();

        let att = Attachment::new("photo.jpg", "image/jpeg", 8, Some(img_path), None);
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, false, false).await;
        assert!(result.images.is_empty());
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("no vision-capable provider is active"));
    }

    #[tokio::test]
    async fn purged_image_advertises_fetch_attachment() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let mut att = Attachment::new(
            "old.png",
            "image/png",
            123,
            None,
            Some(AttachmentSource::Telegram {
                chat_id: 1,
                message_id: 2,
                file_id: "x".into(),
            }),
        );
        att.purged_at = Some(chrono::Utc::now());
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, true, false).await;
        assert!(result.images.is_empty());
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("call fetch_attachment"));
    }

    #[tokio::test]
    async fn pdf_without_sidecar_but_with_local_path_extracts_lazily() {
        // Regression: row inserted with extracted_text_path=None (e.g.
        // email persist's eager extraction skipped or crashed) but the
        // PDF bytes are still on disk. Surfacing should run extraction
        // right then rather than degrading to metadata-only and asking
        // the user to upload again.
        use std::io::Write;
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let dir = std::env::temp_dir().join(format!("athen_att_lazy_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_path = dir.join("hello.pdf");

        // Smallest valid one-page PDF that pdf-extract can parse.
        // Hand-written: header, 4 objects (catalog, pages, page,
        // contents stream with "Hello PDF"), xref, trailer.
        let pdf = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
4 0 obj\n<< /Length 44 >>\nstream\nBT /F1 24 Tf 50 100 Td (Hello PDF) Tj ET\nendstream\nendobj\n\
5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n\
xref\n0 6\n0000000000 65535 f \n0000000010 00000 n \n0000000060 00000 n \n0000000110 00000 n \n0000000220 00000 n \n0000000310 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n380\n%%EOF\n";
        let mut f = std::fs::File::create(&pdf_path).unwrap();
        f.write_all(pdf).unwrap();
        drop(f);

        let att = Attachment::new(
            "hello.pdf",
            "application/pdf",
            pdf.len() as u64,
            Some(pdf_path.clone()),
            Some(AttachmentSource::Email {
                account_id: "primary".into(),
                mailbox: "INBOX".into(),
                uid_validity: 1,
                uid: 1,
                part_path: "1".into(),
            }),
        );
        // NOTE: extracted_text_path intentionally None — simulates
        // failed/skipped eager extraction during email persist.
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, false, false).await;
        let msg = result.system_message.expect("system message");
        // The lazy path either succeeds and inlines text, or fails
        // gracefully. In the failure branch there's nothing for
        // read_attachment_full to read, so the surfacing message must
        // not advertise it (the tool itself exists, but it would only
        // re-confirm "no readable representation").
        if msg.contains("BEGIN extracted text") {
            // Happy path: extraction worked, full content inlined.
            assert!(msg.contains("hello.pdf"));
        } else {
            // Degraded path: pdf-extract failed on this minimal PDF.
            // Don't direct the agent to a tool call that has no payload.
            assert!(msg.contains("only metadata is available"));
            assert!(!msg.contains("call read_attachment_full"));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_mime_lists_metadata_only() {
        let store = fresh_store().await;
        let event_id = Uuid::new_v4();

        let att = Attachment::new(
            "data.bin",
            "application/octet-stream",
            42,
            Some(PathBuf::from("/tmp/data.bin")),
            None,
        );
        store.insert(event_id, &att).await.unwrap();

        let result = prepare_attachment_surfacing(event_id, &store, true, true).await;
        let msg = result.system_message.expect("system message");
        assert!(msg.contains("metadata only"));
    }
}

#[cfg(test)]
mod arc_event_lookup_tests {
    use super::latest_sense_event_id_in_arc;
    use athen_persistence::arcs::{ArcSource, EntryType};
    use athen_persistence::Database;
    use uuid::Uuid;

    #[tokio::test]
    async fn returns_most_recent_event_id_from_metadata() {
        let db = Database::in_memory().await.unwrap();
        let store = db.arc_store();
        let arc_id = Uuid::new_v4().to_string();
        store
            .create_arc(&arc_id, "Email", ArcSource::Email)
            .await
            .unwrap();

        let older_event = Uuid::new_v4();
        let newer_event = Uuid::new_v4();
        store
            .add_entry(
                &arc_id,
                EntryType::Message,
                "system",
                "older email body",
                Some(serde_json::json!({ "event_id": older_event.to_string() })),
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                &arc_id,
                EntryType::Message,
                "system",
                "newer email body",
                Some(serde_json::json!({ "event_id": newer_event.to_string() })),
                None,
            )
            .await
            .unwrap();

        let found = latest_sense_event_id_in_arc(&store, &arc_id)
            .await
            .expect("expected an event id");
        assert_eq!(found, newer_event);
    }

    #[tokio::test]
    async fn returns_none_when_no_metadata() {
        let db = Database::in_memory().await.unwrap();
        let store = db.arc_store();
        let arc_id = Uuid::new_v4().to_string();
        store
            .create_arc(&arc_id, "Chat", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry(&arc_id, EntryType::Message, "user", "hi", None, None)
            .await
            .unwrap();

        let found = latest_sense_event_id_in_arc(&store, &arc_id).await;
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn skips_non_uuid_event_ids() {
        let db = Database::in_memory().await.unwrap();
        let store = db.arc_store();
        let arc_id = Uuid::new_v4().to_string();
        store
            .create_arc(&arc_id, "Email", ArcSource::Email)
            .await
            .unwrap();

        let valid = Uuid::new_v4();
        store
            .add_entry(
                &arc_id,
                EntryType::Message,
                "system",
                "valid",
                Some(serde_json::json!({ "event_id": valid.to_string() })),
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                &arc_id,
                EntryType::Message,
                "system",
                "garbage",
                Some(serde_json::json!({ "event_id": "not-a-uuid" })),
                None,
            )
            .await
            .unwrap();

        // Newest entry's event_id is invalid → walker keeps going and
        // surfaces the older valid one.
        let found = latest_sense_event_id_in_arc(&store, &arc_id)
            .await
            .expect("expected an event id");
        assert_eq!(found, valid);
    }
}

#[cfg(test)]
mod composer_upload_tests {
    use super::*;
    use athen_persistence::arcs::ArcSource;
    use athen_persistence::Database;
    use base64::Engine;

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize_upload_filename("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_upload_filename("CV<final>.pdf"), "CV_final_.pdf");
        assert_eq!(sanitize_upload_filename(""), "file");
        assert_eq!(sanitize_upload_filename("..."), "file");
        assert_eq!(sanitize_upload_filename("normal.pdf"), "normal.pdf");
    }

    #[tokio::test]
    async fn empty_uploads_short_circuits() {
        let db = Database::in_memory().await.unwrap();
        let arc_store = db.arc_store();
        let attachment_store = db.attachment_store();
        attachment_store.init_schema().await.unwrap();
        let arc_id = "arc-empty".to_string();
        arc_store
            .create_arc(&arc_id, "Test", ArcSource::UserInput)
            .await
            .unwrap();

        let result =
            persist_uploaded_attachments(Some(&arc_store), Some(&attachment_store), &arc_id, &[])
                .await
                .unwrap();
        assert!(result.is_none());

        // Confirm: no arc entry stamped, no attachment row inserted.
        let entries = arc_store.load_entries(&arc_id).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn no_attachment_store_returns_none() {
        let db = Database::in_memory().await.unwrap();
        let arc_store = db.arc_store();
        let arc_id = "arc-none".to_string();
        arc_store
            .create_arc(&arc_id, "Test", ArcSource::UserInput)
            .await
            .unwrap();

        let upload = UploadedAttachment {
            name: "x.txt".into(),
            mime_type: "text/plain".into(),
            base64: base64::engine::general_purpose::STANDARD.encode(b"hi"),
        };
        // No AttachmentStore wired (CLI / test path) — must short-circuit
        // gracefully without touching the arc.
        let result = persist_uploaded_attachments(Some(&arc_store), None, &arc_id, &[upload])
            .await
            .unwrap();
        assert!(result.is_none());
        let entries = arc_store.load_entries(&arc_id).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn persists_text_upload_returns_event_id_and_writes_row() {
        let db = Database::in_memory().await.unwrap();
        let arc_store = db.arc_store();
        let attachment_store = db.attachment_store();
        attachment_store.init_schema().await.unwrap();
        let arc_id = "arc-text-upload".to_string();
        arc_store
            .create_arc(&arc_id, "Upload", ArcSource::UserInput)
            .await
            .unwrap();

        let upload = UploadedAttachment {
            name: "note.txt".into(),
            mime_type: "text/plain".into(),
            base64: base64::engine::general_purpose::STANDARD.encode(b"plain note body"),
        };
        let event_id = persist_uploaded_attachments(
            Some(&arc_store),
            Some(&attachment_store),
            &arc_id,
            &[upload],
        )
        .await
        .unwrap()
        .expect("expected a synthesized event id");

        // The AttachmentStore row exists and points to a real on-disk file.
        let rows = attachment_store.list_for_event(event_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "note.txt");
        assert_eq!(row.mime_type, "text/plain");
        let path = row.local_path.as_ref().expect("local_path must be set");
        assert!(path.exists(), "uploaded file must be on disk");
        let body = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(body, "plain note body");

        // The helper itself no longer writes a marker arc entry — the
        // caller is responsible for stamping the user-message entry's
        // metadata with `attachment_event_id`. Verify no spurious arc
        // entries leaked through.
        let entries = arc_store.load_entries(&arc_id).await.unwrap();
        assert!(
            entries.is_empty(),
            "persist_uploaded_attachments must not write arc entries"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn malicious_filename_is_sanitized_on_disk() {
        let db = Database::in_memory().await.unwrap();
        let arc_store = db.arc_store();
        let attachment_store = db.attachment_store();
        attachment_store.init_schema().await.unwrap();
        let arc_id = "arc-bad-name".to_string();
        arc_store
            .create_arc(&arc_id, "X", ArcSource::UserInput)
            .await
            .unwrap();

        let upload = UploadedAttachment {
            name: "../../../etc/passwd".into(),
            mime_type: "text/plain".into(),
            base64: base64::engine::general_purpose::STANDARD.encode(b"x"),
        };
        let event_id = persist_uploaded_attachments(
            Some(&arc_store),
            Some(&attachment_store),
            &arc_id,
            &[upload],
        )
        .await
        .unwrap()
        .unwrap();

        let rows = attachment_store.list_for_event(event_id).await.unwrap();
        let path = rows[0].local_path.as_ref().unwrap();
        // Path separators replaced by underscores: the file lands in
        // exactly one component under the per-event_id directory.
        let parent = path.parent().unwrap();
        assert!(parent.ends_with(event_id.to_string()));
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(!file_name.contains('/'));
        assert!(!file_name.contains('\\'));
        // Defense-in-depth: every traversal `..` becomes `_..` after
        // separator scrubbing, so the on-disk name has no path
        // components that could escape the parent dir.
        assert_eq!(
            std::path::Path::new(file_name.as_ref())
                .components()
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(parent);
    }
}

#[cfg(test)]
mod summary_tests {
    use super::summarize_tool_call;
    use serde_json::json;

    fn s(tool: &str, args: serde_json::Value, result: serde_json::Value) -> Option<String> {
        summarize_tool_call(tool, Some(&args), Some(&result))
    }

    #[test]
    fn read_uses_args_path_even_when_result_omits_it() {
        // The actual `read` tool returns {content, lines_returned, ...} —
        // no `path` field. The old summarizer relied on result.path and
        // returned None, so the UI showed an empty card. Regression-guard
        // that we now read the path out of the args.
        let out = s(
            "read",
            json!({ "path": "/etc/hosts" }),
            json!({ "content": "...", "lines_returned": 5 }),
        );
        assert_eq!(out.as_deref(), Some("/etc/hosts"));
    }

    #[test]
    fn read_includes_offset_and_limit_when_present() {
        let out = s(
            "read",
            json!({ "path": "/big.log", "offset": 100, "limit": 50 }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("/big.log (lines 100–150)"));
    }

    #[test]
    fn list_directory_uses_args_path_and_count() {
        let out = s(
            "list_directory",
            json!({ "path": "/tmp" }),
            json!({ "count": 3 }),
        );
        assert_eq!(out.as_deref(), Some("/tmp (3 entries)"));
        let out_one = s(
            "list_directory",
            json!({ "path": "/tmp" }),
            json!({ "count": 1 }),
        );
        assert_eq!(out_one.as_deref(), Some("/tmp (1 entry)"));
    }

    #[test]
    fn grep_renders_pattern_in_path() {
        let out = s(
            "grep",
            json!({ "pattern": "TODO", "path": "src", "glob": "*.rs" }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("\"TODO\" in src (*.rs)"));
    }

    #[test]
    fn shell_execute_summarises_command_not_stdout() {
        // Old behaviour: dump stdout. New behaviour: show the command —
        // *what the agent did* matters more than how it answered.
        let out = s(
            "shell_execute",
            json!({ "command": "ls -la /tmp" }),
            json!({ "stdout": "lots of output here" }),
        );
        assert_eq!(out.as_deref(), Some("ls -la /tmp"));
    }

    #[test]
    fn write_includes_human_byte_count() {
        let out = s(
            "write",
            json!({ "path": "/tmp/x.md" }),
            json!({ "path": "/tmp/x.md", "bytes_written": 2048 }),
        );
        assert_eq!(out.as_deref(), Some("/tmp/x.md (2.0KB)"));
    }

    #[test]
    fn edit_includes_replacement_count() {
        let out = s(
            "edit",
            json!({ "path": "/tmp/x.md" }),
            json!({ "replacements": 3 }),
        );
        assert_eq!(out.as_deref(), Some("/tmp/x.md (3 edits)"));
    }

    #[test]
    fn create_wakeup_one_shot_relative() {
        let out = s(
            "create_wakeup",
            json!({
                "instruction": "Check the form",
                "schedule": { "kind": "one_shot", "in": "1h" }
            }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("in 1h — Check the form"));
    }

    #[test]
    fn create_wakeup_interval_uses_human_duration() {
        let out = s(
            "create_wakeup",
            json!({
                "instruction": "Refresh stats",
                "schedule": { "kind": "interval", "every_seconds": 3600 }
            }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("every 1h — Refresh stats"));
    }

    #[test]
    fn create_wakeup_cron_passes_expression_through() {
        let out = s(
            "create_wakeup",
            json!({
                "instruction": "Daily news",
                "schedule": { "kind": "cron", "expr": "0 8 * * *" }
            }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("cron: 0 8 * * * — Daily news"));
    }

    #[test]
    fn delegate_to_agent_combines_profile_and_task() {
        let out = s(
            "delegate_to_agent",
            json!({ "profile": "researcher", "task": "Summarise the docs" }),
            json!({}),
        );
        assert_eq!(out.as_deref(), Some("researcher: Summarise the docs"));
    }

    #[test]
    fn unknown_tool_returns_none_so_caller_can_fall_back() {
        let out = s("frobnicate", json!({ "x": 1 }), json!({ "ok": true }));
        assert!(out.is_none());
    }
}
