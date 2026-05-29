//! Generic sense-to-arc router.
//!
//! Processes incoming `SenseEvent`s from any sense monitor (email, calendar,
//! messaging, etc.) by triaging their relevance via LLM, then creating or
//! merging into an Arc.

use std::sync::Arc;

use chrono::Utc;
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use athen_contacts::ContactStore;
use athen_core::event::{EventSource, SenseEvent};
use athen_core::llm::{
    ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent, ModelProfile,
    Role as LlmRole,
};
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_core::traits::llm::LlmRouter;
use athen_core::wakeup::AutonomyBand;
use athen_llm::router::DefaultLlmRouter;
use athen_persistence::arcs::{ArcEntry, ArcMeta, ArcSource, ArcStatus, ArcStore, EntryType};
use athen_persistence::attachments::AttachmentStore;
use athen_persistence::contacts::SqliteContactStore;

use crate::notifier::NotificationOrchestrator;

/// Result of LLM triage for any sense event.
pub struct SenseTriage {
    /// One of: "ignore", "low", "medium", "high"
    pub relevance: String,
    /// One-line explanation
    pub reason: String,
    /// Suggested action: "none", "read", "reply", "calendar", "urgent"
    pub suggested_action: String,
    /// Whether to create a new arc or append to existing
    pub target_arc: TriageTarget,
}

/// Where to route the sense event.
pub enum TriageTarget {
    /// Create a new Arc for this event.
    NewArc { name: String },
    /// Append to an existing Arc by ID.
    ExistingArc { arc_id: String },
}

/// Process a `SenseEvent` through the full pipeline:
/// 1. Triage relevance via LLM
/// 2. Find related existing arc or create new one
/// 3. Persist as ArcEntry
/// 4. Emit frontend event
/// 5. Notify
/// 6. (optional) Hand off to the coordinator for autonomous execution
///
/// Returns `true` if the event was relevant and processed, `false` if ignored.
#[allow(clippy::too_many_arguments)]
pub async fn process_sense_event(
    event: &SenseEvent,
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    arc_store: &Option<ArcStore>,
    profile_store: &Option<Arc<athen_persistence::profiles::SqliteProfileStore>>,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &crate::state::ProfileEmbeddingCache,
    app_handle: &AppHandle,
    notifier: Option<&Arc<NotificationOrchestrator>>,
    coordinator: Option<&Arc<athen_coordinador::Coordinator>>,
    task_arc_map: Option<&crate::state::TaskArcMap>,
    dispatch_signal: Option<&Arc<tokio::sync::Notify>>,
    approval_router: Option<&Arc<crate::approval::ApprovalRouter>>,
    pending_email_marks: Option<&crate::state::PendingEmailMarks>,
    attachment_store: Option<&AttachmentStore>,
    contact_store: Option<&SqliteContactStore>,
    telegram_chat_log: Option<&Arc<athen_persistence::telegram_chat_log::TelegramChatLogStore>>,
) -> bool {
    let source_name = source_display_name(&event.source);
    let summary = event.content.summary.as_deref().unwrap_or("(no subject)");
    let sender = event
        .sender
        .as_ref()
        .map(|s| s.display_name.as_deref().unwrap_or(&s.identifier))
        .unwrap_or(match event.source {
            EventSource::Calendar => "Calendar",
            EventSource::System => "System",
            _ => "unknown",
        });

    // Extract body text — for emails it's in "text", for calendar events
    // we build a readable summary from the structured fields.
    let body_text = if event.source == EventSource::Calendar {
        format_calendar_body(&event.content.body)
    } else {
        event
            .content
            .body
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string()
    };

    // Truncate body for LLM triage (save tokens).
    let body_for_triage: String = if body_text.len() > 1000 {
        let cap = body_text.floor_char_boundary(1000);
        format!("{}...", &body_text[..cap])
    } else {
        body_text.clone()
    };

    // Surface attachments to triage so the LLM doesn't dismiss messages
    // like "Read this PDF" as "no PDF visible" — the bytes ARE on the
    // message, they just don't live in the body text. Without this the
    // triage prompt would only see the body and could (and did) misfire
    // notify-only when the user clearly wanted Athen to read the file.
    let body_for_triage = if event.content.attachments.is_empty() {
        body_for_triage
    } else {
        let summary_lines: Vec<String> = event
            .content
            .attachments
            .iter()
            .map(|a| format!("  - \"{}\" ({}, {}B)", a.name, a.mime_type, a.size_bytes))
            .collect();
        format!(
            "{body_for_triage}\n\n[Attachments on this message ({}):\n{}]",
            event.content.attachments.len(),
            summary_lines.join("\n"),
        )
    };

    // Step 0: Fetch recent active arcs for context matching.
    let recent_arcs = if let Some(store) = arc_store {
        store
            .list_arcs()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.status == ArcStatus::Active)
            .take(10)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Step 0b: Fetch a short body snippet of each candidate arc's recent
    // entries so the triage LLM can disambiguate arcs whose names alone
    // collide. Best-effort: any load failure just falls back to an empty
    // snippet (today's metadata-only behavior).
    let mut recent_arcs_with_snippets: Vec<(ArcMeta, String)> =
        Vec::with_capacity(recent_arcs.len());
    for arc in recent_arcs.iter() {
        let snippet = if let Some(store) = arc_store {
            match store.load_entries(&arc.id).await {
                Ok(entries) => build_arc_snippet(&entries),
                Err(e) => {
                    tracing::debug!(arc = %arc.id, error = %e, "snippet load_entries failed");
                    String::new()
                }
            }
        } else {
            String::new()
        };
        recent_arcs_with_snippets.push((arc.clone(), snippet));
    }

    // Cross-arc chat history for messaging events — gives the triage
    // LLM continuity beyond what any single arc's snippet shows. Only
    // fetched for Messaging source (other sources have no chat_id).
    let messaging_chat_id: Option<i64> = if event.source == EventSource::Messaging {
        event.content.body.get("chat_id").and_then(|v| v.as_i64())
    } else {
        None
    };
    let chat_history: Vec<athen_persistence::telegram_chat_log::TelegramLogEntry> =
        match (telegram_chat_log, messaging_chat_id) {
            (Some(store), Some(cid)) => store.recent(cid, 4).await.unwrap_or_default(),
            _ => Vec::new(),
        };
    // Log this inbound *after* the fetch so the current message
    // doesn't appear in its own context window. Done here (not just in
    // the owner-Telegram path) so non-owner chats also accumulate a
    // transcript the next triage can lean on.
    if let (Some(store), Some(cid)) = (telegram_chat_log, messaging_chat_id) {
        if let Err(e) = store
            .append(
                cid,
                athen_persistence::telegram_chat_log::TelegramLogDirection::Inbound,
                &body_text,
                !event.content.attachments.is_empty(),
            )
            .await
        {
            tracing::warn!(error = %e, chat_id = cid, "telegram_chat_log append (non-owner inbound) failed");
        }
    }

    // Step 1: Triage via LLM (with arc context for matching).
    let triage = triage_event(
        router,
        &event.source,
        sender,
        summary,
        &body_for_triage,
        &recent_arcs_with_snippets,
        &chat_history,
    )
    .await;

    if triage.relevance == "ignore" || triage.relevance == "low" {
        info!(
            "{} from '{}' triaged as {} — skipping: {}",
            source_name, sender, triage.relevance, triage.reason
        );
        return false;
    }

    info!(
        "{} from '{}' triaged as {} — processing",
        source_name, sender, triage.relevance
    );

    // Step 2: Find or create an Arc.
    // Time-window grouping: if the LLM wants a new arc but there's a recent
    // arc from the same source updated within the last 5 minutes, merge into
    // it instead.  This prevents rapid-fire messages from the same sender
    // spawning separate arcs.
    let arc_source = event_source_to_arc_source(&event.source);
    let arc_id = match &triage.target_arc {
        TriageTarget::NewArc { name } => {
            // Check for a recent arc from the same source within the time window.
            let recent_match = find_recent_arc_from_source(&recent_arcs, &arc_source, 300);
            if let Some(existing_id) = recent_match {
                info!(
                    "Merging {} from '{}' into recent arc '{}' (time-window grouping)",
                    source_name, sender, existing_id
                );
                existing_id
            } else {
                let id = generate_arc_id();
                if let Some(store) = arc_store {
                    if let Err(e) = store.create_arc(&id, name, arc_source.clone()).await {
                        warn!("Failed to create arc for sense event: {e}");
                    }
                }
                info!(
                    "Created new arc '{}' for {} from '{}'",
                    id, source_name, sender
                );

                // Route the new arc to a profile based on its source +
                // content. Best-effort: any failure here just leaves the arc
                // on the seeded default profile.
                route_new_arc_to_profile(
                    arc_store.as_ref(),
                    profile_store.as_ref(),
                    profile_embedder,
                    profile_embedding_cache,
                    Some(router),
                    &id,
                    arc_source.as_str(),
                    summary,
                    &body_text,
                )
                .await;

                id
            }
        }
        TriageTarget::ExistingArc { arc_id } => {
            info!(
                "Appending {} from '{}' to existing arc '{}'",
                source_name, sender, arc_id
            );
            arc_id.clone()
        }
    };

    // Step 3: Persist as ArcEntry + context message.
    let entry_type = event_source_to_entry_type(&event.source);
    let entry_content = format_entry_content(sender, summary, &body_text);
    let entry_metadata = serde_json::json!({
        "event_id": event.id.to_string(),
        "source": source_name,
        "sender": sender,
        "subject": summary,
        "relevance": triage.relevance,
        "reason": triage.reason,
        "suggested_action": triage.suggested_action,
    });

    if let Some(store) = arc_store {
        // Store the raw sense event entry.
        if let Err(e) = store
            .add_entry(
                &arc_id,
                entry_type,
                source_name,
                &entry_content,
                Some(entry_metadata.clone()),
                None,
            )
            .await
        {
            warn!("Failed to persist sense event entry: {e}");
        }

        // Also add a system message so the agent has context when the user
        // opens this Arc and starts chatting.
        // `source_risk == Safe` is set upstream (e.g. EmailMonitor matches
        // From against the owner's known addresses; Telegram monitor flags
        // the owner chat) — propagate it so the agent prompt addresses the
        // user directly rather than asking it to triage a stranger.
        let from_owner = matches!(event.source_risk, athen_core::risk::RiskLevel::Safe);

        // Pre-resolve the sender's contact so the agent prompt names the
        // person instead of pushing the raw email/Telegram-user-id into the
        // model and asking it to re-derive what we already know. Best-effort:
        // any error / unknown identifier just falls back to the legacy
        // "Sender identifier: ..." instructions in build_context_message.
        let resolved_contact_name: Option<String> = resolve_sender_contact_name(
            contact_store,
            &event.source,
            event.sender.as_ref(),
            &event.content.body,
        )
        .await;

        let context_msg = build_context_message(
            &event.source,
            sender,
            summary,
            &body_text,
            &event.content.body,
            &triage,
            from_owner,
            resolved_contact_name.as_deref(),
        );
        if let Err(e) = store
            .add_entry(
                &arc_id,
                EntryType::Message,
                "system",
                &context_msg,
                None,
                None,
            )
            .await
        {
            warn!("Failed to persist sense context message: {e}");
        }

        if let Err(e) = store.touch_arc(&arc_id).await {
            warn!("Failed to touch arc: {e}");
        }
    }

    // Persist attachment refs so the executor can inline them on the
    // first turn and so the agent's `read_attachment_full` /
    // `fetch_attachment` tools can resolve by id later. Best-effort:
    // any insert error is logged and skipped — the bytes are already on
    // disk and the agent will still see metadata in the system context
    // message; missing rows just mean refetch-after-purge won't work
    // for that one attachment.
    if let Some(astore) = attachment_store {
        for att in &event.content.attachments {
            if let Err(e) = astore.insert(event.id, att).await {
                warn!(
                    event_id = %event.id,
                    attachment = %att.name,
                    error = %e,
                    "Failed to persist attachment ref"
                );
            }
        }
    }

    // Whether the agent will run autonomously on this event. Computed once
    // here so the frontend event and notification can present accurate UX
    // (no "Draft Reply" button when Athen is already drafting a reply).
    let will_dispatch = coordinator.is_some() && should_dispatch_autonomously(&triage);

    let body_preview: String = if body_text.len() > 500 {
        let cap = body_text.floor_char_boundary(500);
        format!("{}...", &body_text[..cap])
    } else {
        body_text.trim().to_string()
    };

    // Step 4: Hand off to the coordinator for autonomous execution if
    // wiring is present and the triage decided this event warrants
    // action (vs. just notifying the user). We do this BEFORE step 5 so
    // the notification can reflect the actual risk decision (e.g.
    // "Athen wants to act…" when HumanConfirm is returned).
    //
    // Best-effort: failures here never invalidate the work the
    // notification path already did.
    use athen_core::risk::RiskDecision;
    let mut final_decision: Option<RiskDecision> = None;
    let mut pending_human_confirm: Option<athen_core::task::TaskId> = None;

    // Pre-compute the email-mark coordinates once. Any decision that could
    // lead to a successful autonomous run (SilentApprove, NotifyAndProceed,
    // HumanConfirm) stashes these so the dispatch loop can flag the IMAP
    // message `\Seen` after the agent succeeds. HardBlock never runs, so we
    // never stash for it.
    let email_mark_info: Option<crate::state::EmailMarkInfo> = if event.source == EventSource::Email
    {
        let uid = event.raw_id.as_deref().and_then(|s| s.parse::<u32>().ok());
        let folder = event
            .content
            .body
            .get("folder")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        match (uid, folder) {
            (Some(uid), Some(folder)) => Some(crate::state::EmailMarkInfo { uid, folder }),
            _ => {
                tracing::debug!(
                    raw_id = ?event.raw_id,
                    "Email event missing UID or folder; will not auto-mark seen on success"
                );
                None
            }
        }
    } else {
        None
    };

    if let Some(coord) = coordinator {
        if will_dispatch {
            // Effective security posture, snapshotted at event-processing
            // (= task creation) time: per-arc override ⊕ live global.
            let global_security_mode = {
                use tauri::Manager;
                app_handle
                    .state::<crate::state::AppState>()
                    .security
                    .load()
                    .mode
            };
            let security_mode = crate::state::resolve_security_mode_for_arc(
                arc_store.as_ref(),
                &arc_id,
                global_security_mode,
            )
            .await;
            match coord
                .process_event_authorized(event.clone(), AutonomyBand::SafeOnly, security_mode)
                .await
            {
                Ok(decisions) => {
                    for (task_id, decision) in &decisions {
                        info!(
                            arc = %arc_id,
                            task_id = %task_id,
                            ?decision,
                            "Sense event handed to coordinator"
                        );
                        if matches!(
                            decision,
                            RiskDecision::SilentApprove | RiskDecision::NotifyAndProceed
                        ) {
                            if let Some(map) = task_arc_map {
                                map.write().await.insert(*task_id, arc_id.clone());
                            }
                        }
                        // Stash the email-mark coordinates for any decision
                        // that can lead to a successful run. HumanConfirm
                        // is included because the user may approve later;
                        // dispatch loop drops the entry on failure either
                        // way, so a denied/failed task can't leak.
                        if matches!(
                            decision,
                            RiskDecision::SilentApprove
                                | RiskDecision::NotifyAndProceed
                                | RiskDecision::HumanConfirm
                        ) {
                            if let (Some(map), Some(info)) =
                                (pending_email_marks, email_mark_info.as_ref())
                            {
                                map.write().await.insert(*task_id, info.clone());
                            }
                        }
                        if matches!(decision, RiskDecision::HumanConfirm) {
                            pending_human_confirm = Some(*task_id);
                        }
                    }
                    if !decisions.is_empty() {
                        if let Some(sig) = dispatch_signal {
                            sig.notify_one();
                        }
                    }
                    // Take the first decision as authoritative — sense
                    // events realistically map to a single task.
                    final_decision = decisions.first().map(|(_, d)| *d);
                }
                Err(e) => {
                    warn!(arc = %arc_id, error = %e, "coordinator.process_event failed");
                }
            }
        } else {
            tracing::debug!(
                arc = %arc_id,
                relevance = %triage.relevance,
                action = %triage.suggested_action,
                "Sense event triaged as notify-only; not dispatching"
            );
        }
    }

    // Step 5: Emit frontend event. `decision` is informational so the
    // frontend can future-render an "Awaiting your approval" state for
    // HumanConfirm; today's UI just keys off `dispatched`.
    let decision_str = final_decision.as_ref().map(decision_label);
    let _ = app_handle.emit(
        "sense-event",
        serde_json::json!({
            "source": source_name,
            "from": sender,
            "subject": summary,
            "body_preview": body_preview,
            "relevance": triage.relevance,
            "reason": triage.reason,
            "suggested_action": triage.suggested_action,
            "arc_id": arc_id,
            "event_id": event.id.to_string(),
            // When true the frontend hides Draft Reply / Summarize / Add to
            // Calendar buttons — Athen is already acting, so the user-action
            // affordances would be misleading.
            "dispatched": will_dispatch,
            "decision": decision_str,
        }),
    );

    // Step 6: Notify through orchestrator channels — title/body/urgency
    // now reflect the actual risk decision so HumanConfirm doesn't lie
    // about Athen "handling" something it's actually waiting on.
    if let Some(notifier) = notifier {
        let baseline_urgency = match triage.relevance.as_str() {
            "high" => NotificationUrgency::High,
            "medium" => NotificationUrgency::Medium,
            _ => NotificationUrgency::Low,
        };

        let copy = build_sense_notification_copy(
            will_dispatch,
            final_decision.as_ref(),
            source_name,
            sender,
            summary,
            &triage.reason,
            &body_preview,
            baseline_urgency,
        );

        let notification = Notification {
            id: Uuid::new_v4(),
            urgency: copy.urgency,
            title: copy.title,
            body: copy.body,
            origin: NotificationOrigin::SenseRouter,
            arc_id: Some(arc_id.clone()),
            task_id: None,
            created_at: Utc::now(),
            requires_response: false,
            skip_humanize: copy.skip_humanize,
            body_long: None,
        };

        notifier.notify(notification).await;
    }

    // Step 7: HumanConfirm path — fire a real approval question through
    // the cross-channel router. Without this, the task sits in
    // `awaiting_approval` forever and the notification we just sent would
    // be the only signal the user ever gets.
    if let Some(task_id) = pending_human_confirm {
        if let Some(router) = approval_router {
            let router = Arc::clone(router);
            let coordinator = coordinator.cloned();
            let task_arc_map = task_arc_map.cloned();
            let dispatch_signal = dispatch_signal.cloned();
            let arc_id_c = arc_id.clone();
            let source_c = source_name.to_string();
            let sender_c = sender.to_string();
            let summary_c = summary.to_string();
            let reason_c = triage.reason.clone();
            tokio::spawn(async move {
                let prompt = format!("Act on {source_c} from {sender_c}? Subject: {summary_c}.");
                let mut question = athen_core::approval::ApprovalQuestion::approve_or_deny(prompt);
                question.arc_id = Some(arc_id_c.clone());
                question.task_id = Some(task_id);
                if !reason_c.is_empty() {
                    question.description = Some(reason_c);
                }

                let primary = router.pick_primary(Some(&arc_id_c)).await;
                match router.ask_with_escalation(question, primary).await {
                    Ok(answer) => match answer.choice_key.as_str() {
                        "approve" => {
                            let Some(coord) = coordinator else {
                                warn!(
                                    task_id = %task_id,
                                    "HumanConfirm approved but coordinator handle missing"
                                );
                                return;
                            };
                            if let Err(e) = coord.approve_task(task_id).await {
                                warn!(task_id = %task_id, error = %e, "approve_task failed");
                                return;
                            }
                            if let Some(map) = task_arc_map {
                                map.write().await.insert(task_id, arc_id_c.clone());
                            }
                            if let Some(sig) = dispatch_signal {
                                sig.notify_one();
                            }
                            info!(
                                arc = %arc_id_c,
                                task_id = %task_id,
                                "HumanConfirm approved — dispatched"
                            );
                        }
                        "deny" => {
                            if let Some(coord) = coordinator {
                                if let Err(e) = coord.deny_task(task_id).await {
                                    warn!(task_id = %task_id, error = %e, "deny_task failed");
                                }
                            }
                            info!(
                                arc = %arc_id_c,
                                task_id = %task_id,
                                "HumanConfirm denied"
                            );
                        }
                        other => {
                            warn!(
                                task_id = %task_id,
                                choice = %other,
                                "HumanConfirm: unknown choice — leaving task awaiting approval"
                            );
                        }
                    },
                    Err(e) => {
                        warn!(
                            task_id = %task_id,
                            error = %e,
                            "HumanConfirm approval router failed — task left awaiting approval"
                        );
                    }
                }
            });
        } else {
            warn!(
                task_id = %task_id,
                arc = %arc_id,
                "HumanConfirm decision but no approval_router wired — task will sit unactioned"
            );
        }
    }

    true
}

/// Map a `RiskDecision` to a stable lowercase string the frontend can
/// branch on. Kept private and snake_case so the UI doesn't need to know
/// the Rust enum identifiers.
fn decision_label(decision: &athen_core::risk::RiskDecision) -> &'static str {
    use athen_core::risk::RiskDecision;
    match decision {
        RiskDecision::SilentApprove => "silent_approve",
        RiskDecision::NotifyAndProceed => "notify_and_proceed",
        RiskDecision::HumanConfirm => "human_confirm",
        RiskDecision::HardBlock => "hard_block",
    }
}

/// Notification copy chosen based on `(will_dispatch, decision)`.
struct SenseNotifCopy {
    title: String,
    body: String,
    urgency: NotificationUrgency,
    skip_humanize: bool,
}

/// Pure helper: pick title/body/urgency/skip_humanize for a sense-event
/// notification. Factored out so we can unit-test the branching without
/// spinning up a full orchestrator.
#[allow(clippy::too_many_arguments)]
fn build_sense_notification_copy(
    will_dispatch: bool,
    decision: Option<&athen_core::risk::RiskDecision>,
    source_name: &str,
    sender: &str,
    summary: &str,
    reason: &str,
    body_preview: &str,
    baseline_urgency: NotificationUrgency,
) -> SenseNotifCopy {
    use athen_core::risk::RiskDecision;

    // Notify-only path — preserve previous semantics byte-for-byte.
    if !will_dispatch {
        let body = if body_preview.len() > 200 {
            let cap = body_preview.floor_char_boundary(200);
            format!("{}...", &body_preview[..cap])
        } else {
            body_preview.to_string()
        };
        return SenseNotifCopy {
            title: format!("{source_name}: {summary}"),
            body,
            urgency: baseline_urgency,
            skip_humanize: false,
        };
    }

    match decision {
        Some(RiskDecision::SilentApprove | RiskDecision::NotifyAndProceed) => SenseNotifCopy {
            title: format!("Athen is handling {source_name} from {sender}"),
            body: format!("Subject: {summary}"),
            urgency: baseline_urgency,
            skip_humanize: true,
        },
        Some(RiskDecision::HumanConfirm) => {
            let body = if reason.is_empty() {
                "Approve in app or via Telegram.".to_string()
            } else {
                format!("Approve in app or via Telegram. {reason}")
            };
            SenseNotifCopy {
                title: format!("Athen wants to act on {source_name} from {sender}"),
                body,
                urgency: NotificationUrgency::High,
                skip_humanize: true,
            }
        }
        Some(RiskDecision::HardBlock) => SenseNotifCopy {
            title: format!("Athen blocked {source_name} from {sender}"),
            body: if reason.is_empty() {
                String::new()
            } else {
                reason.to_string()
            },
            urgency: NotificationUrgency::High,
            skip_humanize: true,
        },
        None => {
            // Bridge errored. Fall back to notify-only behavior.
            tracing::warn!(
                source = source_name,
                sender = sender,
                "Sense bridge returned no decision; falling back to notify-only copy"
            );
            let body = if body_preview.len() > 200 {
                let cap = body_preview.floor_char_boundary(200);
                format!("{}...", &body_preview[..cap])
            } else {
                body_preview.to_string()
            };
            SenseNotifCopy {
                title: format!("{source_name}: {summary}"),
                body,
                urgency: baseline_urgency,
                skip_humanize: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn source_display_name(source: &EventSource) -> &'static str {
    match source {
        EventSource::Email => "email",
        EventSource::Calendar => "calendar",
        EventSource::Messaging => "message",
        EventSource::UserInput => "user_input",
        EventSource::System => "system",
    }
}

pub(crate) fn event_source_to_arc_source(source: &EventSource) -> ArcSource {
    match source {
        EventSource::Email => ArcSource::Email,
        EventSource::Calendar => ArcSource::Calendar,
        EventSource::Messaging => ArcSource::Messaging,
        EventSource::UserInput => ArcSource::UserInput,
        EventSource::System => ArcSource::System,
    }
}

pub(crate) fn event_source_to_entry_type(source: &EventSource) -> EntryType {
    match source {
        EventSource::Email => EntryType::EmailEvent,
        EventSource::Calendar => EntryType::CalendarEvent,
        EventSource::Messaging => EntryType::Message,
        EventSource::UserInput => EntryType::Message,
        EventSource::System => EntryType::SystemEvent,
    }
}

pub(crate) fn generate_arc_id() -> String {
    chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string()
}

/// Build a readable summary from a calendar event's JSON body.
fn format_calendar_body(body: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if let Some(title) = body.get("title").and_then(|t| t.as_str()) {
        parts.push(format!("Event: {title}"));
    }
    if let Some(start) = body.get("start_time").and_then(|t| t.as_str()) {
        parts.push(format!("Starts: {start}"));
    }
    if let Some(end) = body
        .get("end_time")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Ends: {end}"));
    }
    if let Some(loc) = body
        .get("location")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Location: {loc}"));
    }
    if let Some(desc) = body
        .get("description")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Description: {desc}"));
    }
    if let Some(cat) = body
        .get("category")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Category: {cat}"));
    }
    if let Some(mins) = body.get("minutes_until").and_then(|t| t.as_i64()) {
        if mins <= 0 {
            parts.push("Status: Starting now!".to_string());
        } else if mins < 60 {
            parts.push(format!("Status: Starting in {mins} minutes"));
        } else {
            let hours = mins / 60;
            let remaining = mins % 60;
            if remaining == 0 {
                parts.push(format!("Status: Starting in {hours} hour(s)"));
            } else {
                parts.push(format!("Status: Starting in {hours}h {remaining}m"));
            }
        }
    }

    if parts.is_empty() {
        "(calendar event)".to_string()
    } else {
        parts.join("\n")
    }
}

/// Resolve the sender's contact via the contact store, returning the
/// contact's display name (`Contact::name`) if found.
///
/// Returns `None` when:
/// - The contact store isn't wired (CLI / tests).
/// - The event has no sender (Calendar, System).
/// - `find_by_identifier` returns Ok(None) — sender isn't in contacts.
/// - `find_by_identifier` returns Err — logged at debug, treated as miss.
///
/// For Telegram, prefers `body.sender_user_id` (the canonical numeric id)
/// over `sender.identifier`. The Telegram monitor populates both with the
/// same value today, but reading from `body` makes the contract explicit
/// — that's the field that names the canonical identifier.
async fn resolve_sender_contact_name(
    contact_store: Option<&SqliteContactStore>,
    source: &EventSource,
    sender_info: Option<&athen_core::event::SenderInfo>,
    body_json: &serde_json::Value,
) -> Option<String> {
    let store = contact_store?;

    // Pick the identifier value to look up. Email is straightforward
    // (already lowercased upstream). Telegram canonicalises on the numeric
    // user id; prefer body.sender_user_id since that's the documented
    // source of truth and falls back to sender.identifier.
    let identifier: String = match source {
        EventSource::Email => sender_info?.identifier.clone(),
        EventSource::Messaging => body_json
            .get("sender_user_id")
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .or_else(|| sender_info.map(|s| s.identifier.clone()))?,
        // Calendar / UserInput / System have no human sender to resolve.
        _ => return None,
    };

    if identifier.is_empty() {
        return None;
    }

    match store.find_by_identifier(&identifier).await {
        Ok(Some(contact)) => {
            let name = contact.name.trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        }
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(
                identifier = %identifier,
                error = %e,
                "Sender contact lookup failed; falling back to raw identifier"
            );
            None
        }
    }
}

/// Build a system context message that gives the agent full awareness of
/// the sense event when the user opens this Arc.
///
/// `resolved_contact_name` is the contact-store-resolved display name for
/// the sender, when known. When `Some`, the prompt names the contact and
/// drops the "check if this sender exists in contacts" instructions —
/// we already know who they are, so the agent doesn't need to re-derive
/// it via `contacts_search`. When `None`, the prompt falls back to the
/// raw `Sender identifier:` block + contact-search hint.
#[allow(clippy::too_many_arguments)]
fn build_context_message(
    source: &EventSource,
    sender: &str,
    subject: &str,
    body_text: &str,
    body_json: &serde_json::Value,
    triage: &SenseTriage,
    from_owner: bool,
    resolved_contact_name: Option<&str>,
) -> String {
    let source_name = source_display_name(source);
    let mut msg = format!(
        "[{} notification — relevance: {}, suggested action: {}]\n\n",
        source_name, triage.relevance, triage.suggested_action
    );

    match source {
        EventSource::Calendar => {
            msg.push_str(&format!("Calendar reminder: {subject}\n{body_text}"));
            msg.push_str(
                "\n\nThe user may ask you about this event, want to reschedule it, \
                          or need help preparing for it.",
            );
            // User-authored calendar instruction (Settings → Calendar →
            // "Prompt for every event"). Empty by default; when set, this
            // is the one place the user can steer the agent on every
            // reminder without editing code.
            let user_prompt = crate::settings::load_main_config_public()
                .calendar
                .agent_prompt;
            let user_prompt = user_prompt.trim();
            if !user_prompt.is_empty() {
                msg.push_str("\n\nUser standing instruction for calendar events:\n");
                msg.push_str(user_prompt);
            }
        }
        EventSource::Email => {
            if from_owner {
                if let Some(name) = resolved_contact_name {
                    msg.push_str(&format!(
                        "Email from {name} (you)\nSubject: {subject}\n\n{body_text}"
                    ));
                    msg.push_str(
                        "\n\nThis email arrived in your own inbox — Athen recognizes it as \
                         coming from you. Act on it as you would on a direct request from the user.",
                    );
                } else {
                    msg.push_str(&format!(
                        "Email from you (the owner)\nSubject: {subject}\n\n{body_text}"
                    ));
                    msg.push_str(
                        "\n\nThis email arrived in your own inbox — Athen recognizes it as \
                         coming from you. Act on it as you would on a direct request from the user.",
                    );
                }
            } else if let Some(name) = resolved_contact_name {
                msg.push_str(&format!(
                    "Email from {name} <{sender}>\nSubject: {subject}\n\n{body_text}"
                ));
                msg.push_str(&format!(
                    "\n\nThis sender is in your contacts as \"{name}\". \
                     The user may ask you to summarize, reply, or take action on this email."
                ));
            } else {
                msg.push_str(&format!(
                    "Email from {sender}\nSubject: {subject}\n\n{body_text}"
                ));
                msg.push_str(&format!(
                    "\n\nSender identifier: {sender} (type: Email)\n\
                     The user may ask you to summarize, reply, or take action on this email.\n\
                     If you have contacts tools, check if this sender exists in contacts — \
                     if not, consider creating a contact or asking the user if they match an existing one."
                ));
            }
        }
        EventSource::Messaging => {
            if from_owner {
                if let Some(name) = resolved_contact_name {
                    msg.push_str(&format!("Message from {name} (you)\n\n{body_text}"));
                    msg.push_str(
                        "\n\nMessage from you (owner) — Athen recognizes you across channels. \
                         Act on it as you would on a direct request from the user.",
                    );
                } else {
                    msg.push_str(&format!("Message from you (owner)\n\n{body_text}"));
                    msg.push_str(
                        "\n\nMessage from you (owner) — Athen recognizes you across channels. \
                         Act on it as you would on a direct request from the user.",
                    );
                }
            } else if let Some(name) = resolved_contact_name {
                msg.push_str(&format!("Message from {name}\n\n{body_text}"));
                // Keep the Telegram-specific identifier breakdown — username,
                // first_name — useful for the agent even when we know the
                // contact (e.g. the agent may want to @-mention them).
                let tg_user_id = body_json.get("sender_user_id").and_then(|v| v.as_i64());
                let tg_username = body_json.get("sender_username").and_then(|v| v.as_str());
                let tg_name = body_json.get("sender_first_name").and_then(|v| v.as_str());
                let mut sender_details = format!("\n\nSender: {name}");
                if let Some(uid) = tg_user_id {
                    sender_details.push_str(&format!(" | Telegram user ID: {uid}"));
                }
                if let Some(uname) = tg_username {
                    sender_details.push_str(&format!(" | Telegram username: @{uname}"));
                }
                if let Some(tg) = tg_name {
                    sender_details.push_str(&format!(" | Name: {tg}"));
                }
                msg.push_str(&sender_details);
                msg.push_str(&format!(
                    "\n\nThis sender is in your contacts as \"{name}\"."
                ));
            } else {
                msg.push_str(&format!("Message from {sender}\n\n{body_text}"));
                // Extract Telegram-specific sender details from the body JSON.
                let tg_user_id = body_json.get("sender_user_id").and_then(|v| v.as_i64());
                let tg_username = body_json.get("sender_username").and_then(|v| v.as_str());
                let tg_name = body_json.get("sender_first_name").and_then(|v| v.as_str());
                let mut sender_details = format!("\n\nSender: {sender}");
                if let Some(uid) = tg_user_id {
                    sender_details.push_str(&format!(" | Telegram user ID: {uid}"));
                }
                if let Some(uname) = tg_username {
                    sender_details.push_str(&format!(" | Telegram username: @{uname}"));
                }
                if let Some(name) = tg_name {
                    sender_details.push_str(&format!(" | Name: {name}"));
                }
                msg.push_str(&sender_details);
                msg.push_str(
                    "\n\nIf you have contacts tools, check if this sender exists in contacts — \
                     if not, consider creating a contact or asking the user if they match an existing one."
                );
            }
        }
        _ => {
            msg.push_str(&format!(
                "From: {sender}\nSubject: {subject}\n\n{body_text}"
            ));
        }
    }

    if !triage.reason.is_empty() {
        msg.push_str(&format!("\n\nTriage reason: {}", triage.reason));
    }

    msg
}

/// Build a short snippet from an arc's recent entries, suitable for feeding
/// to the triage LLM as topic-disambiguation context.
///
/// Picks the last 2 entries whose `entry_type` is content-bearing (Message,
/// EmailEvent, CalendarEvent — Messaging maps to Message), formats each as
/// `"{source}: {content[..150]}"`, joins with ` | `, and caps the result at
/// 300 chars on a char boundary with a `...` suffix.
pub(crate) fn build_arc_snippet(entries: &[ArcEntry]) -> String {
    let filtered: Vec<&ArcEntry> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.entry_type,
                EntryType::Message | EntryType::EmailEvent | EntryType::CalendarEvent
            )
        })
        .collect();

    // Take the last 2 in chronological order.
    let n = filtered.len();
    let start = n.saturating_sub(2);
    let parts: Vec<String> = filtered[start..]
        .iter()
        .map(|e| {
            let trimmed = e.content.trim();
            let cap = trimmed.floor_char_boundary(150);
            // Replace newlines with spaces so the snippet stays one line in
            // the prompt — multi-line snippets confuse the bullet list.
            let oneline = trimmed[..cap].replace(['\n', '\r'], " ");
            // Escape stray double-quotes so we don't break the surrounding
            // quoted string in the prompt.
            let escaped = oneline.replace('"', "'");
            format!("{}: {}", e.source, escaped.trim())
        })
        .collect();

    let joined = parts.join(" | ");
    if joined.is_empty() {
        return String::new();
    }
    if joined.chars().count() <= 300 {
        return joined;
    }
    let cap = joined.floor_char_boundary(300);
    format!("{}...", &joined[..cap])
}

/// Find a recent active arc from the same source updated within `window_secs` seconds.
///
/// Used for time-window grouping: rapid messages from the same source (e.g.
/// Telegram, Email) get merged into the same arc instead of creating new ones.
fn find_recent_arc_from_source(
    arcs: &[ArcMeta],
    source: &ArcSource,
    window_secs: i64,
) -> Option<String> {
    let now = chrono::Utc::now();
    arcs.iter()
        .filter(|a| a.source == *source && a.status == ArcStatus::Active)
        .find(|a| {
            if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&a.updated_at) {
                let age = now.signed_duration_since(updated);
                age.num_seconds() < window_secs
            } else {
                false
            }
        })
        .map(|a| a.id.clone())
}

/// Best-effort: classify a freshly-created arc and assign the
/// best-matching `AgentProfile` to it. Any failure (no profile store, lookup
/// error, no positive match) leaves the arc on the default profile —
/// today's behavior, fail-open.
///
/// We pass `summary + body_text` to the classifier so keyword matching has
/// real content. The arc's source channel is the strongest signal and
/// drives the domain tag directly.
/// `pub(crate)` so the owner-Telegram fast-path in `state.rs` (which
/// bypasses `process_sense_event`) can also route freshly-created arcs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn route_new_arc_to_profile(
    arc_store: Option<&ArcStore>,
    profile_store: Option<&Arc<athen_persistence::profiles::SqliteProfileStore>>,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &crate::state::ProfileEmbeddingCache,
    llm_router: Option<&Arc<RwLock<Arc<DefaultLlmRouter>>>>,
    arc_id: &str,
    source: &str,
    summary: &str,
    body_text: &str,
) {
    use athen_core::profile_routing::{
        classify_task, pick_profile_blended, profile_embedding_text,
    };
    use athen_core::traits::profile::ProfileStore;

    let (Some(astore), Some(pstore)) = (arc_store, profile_store) else {
        return;
    };

    // Classify from source + (summary, body) concatenated.
    let combined_text = format!("{summary}\n{body_text}");
    let classified = classify_task(Some(source), &combined_text);

    // Fetch all profiles. If only the default exists, there's nothing to
    // route to — leave the arc on default (None).
    let profiles = match pstore.list_profiles().await {
        Ok(list) => list,
        Err(e) => {
            warn!("Profile router: list_profiles failed: {e}");
            return;
        }
    };
    if profiles
        .iter()
        .filter(|p| p.id != athen_core::agent_profile::AgentProfile::DEFAULT_ID)
        .count()
        == 0
    {
        return;
    }

    // Best-effort: build the query embedding + per-profile embeddings.
    // Any embedder error or missing entry just falls back to keyword-only
    // scoring — `pick_profile_blended` tolerates partial coverage.
    let query_embedding = match profile_embedder.embed(&combined_text).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("Profile router: query embedding failed: {e}");
            None
        }
    };

    let mut profile_embeddings: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    if query_embedding.is_some() {
        for p in &profiles {
            // Cache hit: same id + same updated_at.
            {
                let cache = profile_embedding_cache.read().await;
                if let Some((cached_at, vec)) = cache.get(&p.id) {
                    if *cached_at == p.updated_at {
                        profile_embeddings.insert(p.id.clone(), vec.clone());
                        continue;
                    }
                }
            }
            // Miss or stale: embed and write back.
            let text = profile_embedding_text(p);
            if text.is_empty() {
                continue;
            }
            match profile_embedder.embed(&text).await {
                Ok(vec) => {
                    let mut cache = profile_embedding_cache.write().await;
                    cache.insert(p.id.clone(), (p.updated_at, vec.clone()));
                    profile_embeddings.insert(p.id.clone(), vec);
                }
                Err(e) => {
                    tracing::debug!("Profile router: embed for profile {} failed: {e}", p.id);
                }
            }
        }
    }

    let blended = pick_profile_blended(
        &classified,
        &profiles,
        query_embedding.as_deref(),
        &profile_embeddings,
    );

    let decision = if let Some(d) = blended {
        d
    } else if let Some(router_handle) = llm_router {
        // Tier 3: ask an LLM to classify when keyword + semantic both
        // missed. This is where multilingual / generic-phrasing arcs that
        // the heuristics can't catch get a second chance.
        info!(
            arc = arc_id,
            source = source,
            "Profile router: heuristics returned no match; asking LLM classifier"
        );
        let router_arc = router_handle.read().await.clone();
        match athen_core::profile_routing::classify_with_llm(
            router_arc.as_ref(),
            &combined_text,
            &profiles,
        )
        .await
        {
            Some(d) => d,
            None => {
                info!(
                    arc = arc_id,
                    source = source,
                    "Profile router: LLM classifier also returned no match; leaving arc on default"
                );
                return;
            }
        }
    } else {
        info!(
            arc = arc_id,
            source = source,
            domain = ?classified.domain,
            kind = ?classified.kind,
            "Profile router: no positive match (no LLM router available); leaving arc on default"
        );
        return;
    };

    // Compact one-line breakdown of every candidate, e.g.
    //   "outreach: kw=5 sem=0.78 blended=8.12 KEPT, coder: kw=0 sem=0.20 blended=0.80 drop"
    // Lets you eyeball at a glance how close the runner-up was.
    let candidates_breakdown = decision
        .candidates
        .iter()
        .map(|c| {
            format!(
                "{}: kw={} sem={:.2} blended={:.2} {}",
                c.profile_id,
                c.kw,
                c.semantic,
                c.blended,
                if c.kept { "KEPT" } else { "drop" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    info!(
        arc = arc_id,
        winner = decision.profile_id,
        score = decision.score,
        domain = ?classified.domain,
        kind = ?classified.kind,
        candidates = %candidates_breakdown,
        "Profile router: arc → {} ({})",
        decision.profile_id,
        decision.reason
    );

    if let Err(e) = astore
        .set_active_profile_id(arc_id, Some(&decision.profile_id))
        .await
    {
        warn!("Profile router: set_active_profile_id failed: {e}");
    }
}

pub(crate) fn format_entry_content(sender: &str, subject: &str, body: &str) -> String {
    let preview = if body.len() > 500 {
        let cap = body.floor_char_boundary(500);
        format!("{}...", &body[..cap])
    } else {
        body.trim().to_string()
    };
    format!("From: {sender}\nSubject: {subject}\n\n{preview}")
}

// ---------------------------------------------------------------------------
// LLM Triage (generic for any sense)
// ---------------------------------------------------------------------------

/// Triage a sense event via LLM. Returns relevance + suggested action + target arc.
///
/// When `recent_arcs` is non-empty, the LLM is also asked whether the event
/// belongs to an existing arc (by ID) or requires a new one.
///
/// `chat_history`, when non-empty, is rendered as a "Recent exchange with
/// this chat:" block above the new message. Used for `EventSource::Messaging`
/// where the cross-chat transcript helps decide arc continuity even when
/// per-arc snippets are misleading (e.g. the right arc lives in a *different*
/// arc than the most-recently-updated one).
async fn triage_event(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    source: &EventSource,
    sender: &str,
    subject: &str,
    body: &str,
    recent_arcs: &[(ArcMeta, String)],
    chat_history: &[athen_persistence::telegram_chat_log::TelegramLogEntry],
) -> SenseTriage {
    let source_name = source_display_name(source);

    // Build the existing arcs context for the prompt.
    let arcs_context = if recent_arcs.is_empty() {
        String::new()
    } else {
        let mut ctx = String::from("\n\nExisting active Arcs (conversations/threads):\n");
        for (arc, snippet) in recent_arcs {
            let source_label = arc.source.as_str();
            if snippet.is_empty() {
                ctx.push_str(&format!(
                    "- ID: \"{}\" | Name: \"{}\" | Source: {} | Entries: {}\n",
                    arc.id, arc.name, source_label, arc.entry_count,
                ));
            } else {
                ctx.push_str(&format!(
                    "- ID: \"{}\" | Name: \"{}\" | Source: {} | Entries: {} | Recent: \"{}\"\n",
                    arc.id, arc.name, source_label, arc.entry_count, snippet,
                ));
            }
        }
        ctx
    };

    // Cross-arc chat transcript for messaging events — gives the LLM
    // continuity beyond what any single arc's `Recent:` snippet shows.
    let history_context = if chat_history.is_empty() {
        String::new()
    } else {
        let mut ctx = String::from("\n\nRecent exchange with this chat (newest last):\n");
        for entry in chat_history {
            let who = match entry.direction {
                athen_persistence::telegram_chat_log::TelegramLogDirection::Inbound => "them",
                athen_persistence::telegram_chat_log::TelegramLogDirection::Outbound => "us",
            };
            ctx.push_str(&format!("  [{}] {}: {}\n", entry.ts, who, entry.text));
        }
        ctx.push_str(
            "Use this to judge whether the new message is a continuation of an existing arc.\n",
        );
        ctx
    };

    let arc_matching_instruction = if recent_arcs.is_empty() {
        r#"For "arc_name": give a short, descriptive name summarizing the topic (max 40 chars, e.g. "Meeting with John", "Server alert")."#.to_string()
    } else {
        r#"IMPORTANT — Arc matching:
Look at the existing Arcs listed above. If this message is CLEARLY related to one of them (same topic, same person, same thread, a reply to an ongoing conversation), set "existing_arc_id" to that arc's ID.
Use the Recent snippet to verify topic match — names alone can be misleading.
Only create a new arc ("arc_name") if the message is about a genuinely new topic not covered by any existing arc.
When in doubt, prefer creating a new arc over merging into the wrong one.

Set EITHER "existing_arc_id" OR "arc_name", never both."#.to_string()
    };

    let prompt = format!(
        r#"You are a personal assistant triaging an incoming {source_name}.

From: {sender}
Subject: {subject}
Body:
{body}{history_context}{arcs_context}
Respond with ONLY a JSON object (no markdown, no explanation):
{{
  "relevance": "ignore|low|medium|high",
  "reason": "one-line explanation",
  "suggested_action": "none|read|reply|calendar|urgent",
  "arc_name": "short descriptive name for this thread (max 40 chars)",
  "existing_arc_id": null
}}

Classification rules:
- "ignore": ONLY for obvious machine-generated spam, marketing newsletters, automated CI/CD notifications, promotional bulk email
- "low": mailing list digests, non-urgent automated updates, social media notifications
- "medium": any message from a real person, work-related, personal messages, requests, questions, invitations
- "high": urgent requests, deadlines, time-sensitive matters, security alerts

IMPORTANT: Default to "medium" unless you are very confident it is spam or automated. Real messages from real people should NEVER be classified as "ignore".

If the body is followed by an "[Attachments on this message ...]" block, those files ARE accessible to the agent — never say "no attachment is visible" in your reason. A message asking the agent to act on an attachment is a strong signal for "reply" or "urgent".

{arc_matching_instruction}"#
    );

    let request = LlmRequest {
        messages: vec![LlmChatMessage {
            role: LlmRole::User,
            content: LlmContent::Text(prompt),
        }],
        profile: ModelProfile::Cheap,
        max_tokens: Some(200),
        temperature: Some(0.1),
        tools: None,
        system_prompt: None,
        reasoning_effort: athen_core::llm::ReasoningEffort::default(),
    };

    let llm_router = router.read().await.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        llm_router.route(&request),
    )
    .await;

    match result {
        Ok(Ok(response)) => {
            let text = response.content.trim().to_string();
            info!("Sense triage LLM response: {}", text);
            parse_triage_response(&text)
        }
        Ok(Err(e)) => {
            warn!("Sense triage LLM error: {e}");
            SenseTriage {
                relevance: "medium".into(),
                reason: "Could not assess — showing to be safe".into(),
                suggested_action: "read".into(),
                target_arc: TriageTarget::NewArc {
                    name: format!("{} from {}", source_name, sender),
                },
            }
        }
        Err(_) => {
            warn!("Sense triage LLM timed out");
            SenseTriage {
                relevance: "medium".into(),
                reason: "Triage timed out — showing to be safe".into(),
                suggested_action: "read".into(),
                target_arc: TriageTarget::NewArc {
                    name: format!("{} from {}", source_name, sender),
                },
            }
        }
    }
}

/// Parse the LLM's JSON triage response.
pub(crate) fn parse_triage_response(text: &str) -> SenseTriage {
    let cleaned = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .unwrap_or(text)
        .trim()
        .strip_suffix("```")
        .unwrap_or(text)
        .trim();

    match serde_json::from_str::<serde_json::Value>(cleaned) {
        Ok(v) => {
            let relevance = v
                .get("relevance")
                .and_then(|r| r.as_str())
                .unwrap_or("medium")
                .to_string();
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("No reason provided")
                .to_string();
            let suggested_action = v
                .get("suggested_action")
                .and_then(|r| r.as_str())
                .unwrap_or("read")
                .to_string();

            // Check if LLM matched to an existing arc.
            let target_arc = if let Some(arc_id) = v
                .get("existing_arc_id")
                .and_then(|r| r.as_str())
                .filter(|s| !s.is_empty())
            {
                info!("LLM matched sense event to existing arc: {}", arc_id);
                TriageTarget::ExistingArc {
                    arc_id: arc_id.to_string(),
                }
            } else {
                let arc_name = v
                    .get("arc_name")
                    .and_then(|r| r.as_str())
                    .unwrap_or("Incoming event")
                    .to_string();
                TriageTarget::NewArc { name: arc_name }
            };

            SenseTriage {
                relevance,
                reason,
                suggested_action,
                target_arc,
            }
        }
        Err(e) => {
            warn!("Failed to parse triage JSON '{}': {e}", cleaned);
            SenseTriage {
                relevance: "medium".into(),
                reason: "Could not parse triage — showing to be safe".into(),
                suggested_action: "read".into(),
                target_arc: TriageTarget::NewArc {
                    name: "Incoming event".into(),
                },
            }
        }
    }
}

/// Decide whether a triaged sense event should be dispatched to the
/// coordinator for autonomous execution, or merely surfaced as a
/// notification + arc entry.
///
/// We dispatch only when the triage indicates the event is both
/// **relevant** (medium/high) and **action-worthy** (the LLM suggested
/// reply/calendar/urgent — i.e. something the agent can plausibly do).
/// Pure read-only stuff still creates an arc entry and notifies, but
/// stops there. Spam/marketing (`ignore`/`low`) never gets here in the
/// first place; the early bail-out at Step 1 already filtered it out.
pub(crate) fn should_dispatch_autonomously(triage: &SenseTriage) -> bool {
    matches!(triage.relevance.as_str(), "medium" | "high")
        && matches!(
            triage.suggested_action.as_str(),
            "reply" | "calendar" | "urgent"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_triage(relevance: &str, action: &str) -> SenseTriage {
        SenseTriage {
            relevance: relevance.into(),
            reason: "test".into(),
            suggested_action: action.into(),
            target_arc: TriageTarget::NewArc {
                name: "test".into(),
            },
        }
    }

    #[test]
    fn dispatch_gate_lets_action_worthy_events_through() {
        assert!(should_dispatch_autonomously(&make_triage(
            "medium", "reply"
        )));
        assert!(should_dispatch_autonomously(&make_triage("high", "reply")));
        assert!(should_dispatch_autonomously(&make_triage("high", "urgent")));
        assert!(should_dispatch_autonomously(&make_triage(
            "medium", "calendar"
        )));
    }

    #[test]
    fn dispatch_gate_blocks_notify_only_actions() {
        // Read-only / no-action triage should never autonomously execute.
        assert!(!should_dispatch_autonomously(&make_triage(
            "medium", "read"
        )));
        assert!(!should_dispatch_autonomously(&make_triage("high", "read")));
        assert!(!should_dispatch_autonomously(&make_triage(
            "medium", "none"
        )));
    }

    #[test]
    fn dispatch_gate_blocks_low_relevance() {
        // Even action-worthy hints don't dispatch if the LLM thinks
        // the event is irrelevant. (`ignore`/`low` events are skipped
        // before they ever reach this helper, but we belt-and-brace.)
        assert!(!should_dispatch_autonomously(&make_triage(
            "ignore", "reply"
        )));
        assert!(!should_dispatch_autonomously(&make_triage("low", "urgent")));
    }

    #[test]
    fn parse_new_arc_response() {
        let json = r#"{"relevance":"medium","reason":"Work email","suggested_action":"read","arc_name":"Project update from Bob"}"#;
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "medium");
        assert_eq!(triage.reason, "Work email");
        assert_eq!(triage.suggested_action, "read");
        match &triage.target_arc {
            TriageTarget::NewArc { name } => assert_eq!(name, "Project update from Bob"),
            TriageTarget::ExistingArc { .. } => panic!("Expected NewArc"),
        }
    }

    #[test]
    fn parse_existing_arc_response() {
        let json = r#"{"relevance":"high","reason":"Reply to ongoing thread","suggested_action":"reply","existing_arc_id":"arc_20260404_120000","arc_name":null}"#;
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "high");
        assert_eq!(triage.suggested_action, "reply");
        match &triage.target_arc {
            TriageTarget::ExistingArc { arc_id } => assert_eq!(arc_id, "arc_20260404_120000"),
            TriageTarget::NewArc { .. } => panic!("Expected ExistingArc"),
        }
    }

    #[test]
    fn parse_existing_arc_null_falls_back_to_new() {
        let json = r#"{"relevance":"medium","reason":"New topic","suggested_action":"read","existing_arc_id":null,"arc_name":"Server monitoring alert"}"#;
        let triage = parse_triage_response(json);
        match &triage.target_arc {
            TriageTarget::NewArc { name } => assert_eq!(name, "Server monitoring alert"),
            TriageTarget::ExistingArc { .. } => {
                panic!("Expected NewArc when existing_arc_id is null")
            }
        }
    }

    #[test]
    fn parse_existing_arc_empty_string_falls_back_to_new() {
        let json = r#"{"relevance":"medium","reason":"test","suggested_action":"read","existing_arc_id":"","arc_name":"Something new"}"#;
        let triage = parse_triage_response(json);
        match &triage.target_arc {
            TriageTarget::NewArc { name } => assert_eq!(name, "Something new"),
            TriageTarget::ExistingArc { .. } => {
                panic!("Expected NewArc when existing_arc_id is empty")
            }
        }
    }

    #[test]
    fn parse_with_markdown_code_fences() {
        let json = "```json\n{\"relevance\":\"high\",\"reason\":\"Urgent\",\"suggested_action\":\"urgent\",\"arc_name\":\"Critical alert\"}\n```";
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "high");
        assert_eq!(triage.reason, "Urgent");
        match &triage.target_arc {
            TriageTarget::NewArc { name } => assert_eq!(name, "Critical alert"),
            _ => panic!("Expected NewArc"),
        }
    }

    #[test]
    fn parse_with_bare_code_fences() {
        let json = "```\n{\"relevance\":\"low\",\"reason\":\"Newsletter\",\"suggested_action\":\"none\",\"arc_name\":\"test\"}\n```";
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "low");
    }

    #[test]
    fn parse_invalid_json_falls_back_to_medium() {
        let triage = parse_triage_response("this is not json at all");
        assert_eq!(triage.relevance, "medium");
        assert!(triage.reason.contains("Could not parse"));
        match &triage.target_arc {
            TriageTarget::NewArc { .. } => {}
            _ => panic!("Expected NewArc fallback"),
        }
    }

    #[test]
    fn parse_missing_fields_uses_defaults() {
        let json = r#"{"relevance":"high"}"#;
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "high");
        assert_eq!(triage.reason, "No reason provided");
        assert_eq!(triage.suggested_action, "read");
        match &triage.target_arc {
            TriageTarget::NewArc { name } => assert_eq!(name, "Incoming event"),
            _ => panic!("Expected NewArc with default name"),
        }
    }

    #[test]
    fn parse_ignore_relevance() {
        let json = r#"{"relevance":"ignore","reason":"Spam newsletter","suggested_action":"none","arc_name":"spam"}"#;
        let triage = parse_triage_response(json);
        assert_eq!(triage.relevance, "ignore");
        assert_eq!(triage.suggested_action, "none");
    }

    #[test]
    fn source_display_names_correct() {
        assert_eq!(source_display_name(&EventSource::Email), "email");
        assert_eq!(source_display_name(&EventSource::Calendar), "calendar");
        assert_eq!(source_display_name(&EventSource::Messaging), "message");
        assert_eq!(source_display_name(&EventSource::UserInput), "user_input");
        assert_eq!(source_display_name(&EventSource::System), "system");
    }

    #[test]
    fn event_source_maps_to_arc_source() {
        assert_eq!(
            event_source_to_arc_source(&EventSource::Email),
            ArcSource::Email
        );
        assert_eq!(
            event_source_to_arc_source(&EventSource::Calendar),
            ArcSource::Calendar
        );
        assert_eq!(
            event_source_to_arc_source(&EventSource::UserInput),
            ArcSource::UserInput
        );
    }

    #[test]
    fn event_source_maps_to_entry_type() {
        assert_eq!(
            event_source_to_entry_type(&EventSource::Email),
            EntryType::EmailEvent
        );
        assert_eq!(
            event_source_to_entry_type(&EventSource::Calendar),
            EntryType::CalendarEvent
        );
        assert_eq!(
            event_source_to_entry_type(&EventSource::Messaging),
            EntryType::Message
        );
        assert_eq!(
            event_source_to_entry_type(&EventSource::System),
            EntryType::SystemEvent
        );
    }

    #[test]
    fn format_entry_content_truncates_long_body() {
        let long_body = "x".repeat(1000);
        let content = format_entry_content("alice@test.com", "Hello", &long_body);
        assert!(content.contains("From: alice@test.com"));
        assert!(content.contains("Subject: Hello"));
        assert!(content.len() < 1000); // should be truncated
        assert!(content.ends_with("..."));
    }

    #[test]
    fn format_entry_content_short_body() {
        let content = format_entry_content("bob@test.com", "Hi", "Short body");
        assert!(content.contains("From: bob@test.com"));
        assert!(content.contains("Subject: Hi"));
        assert!(content.contains("Short body"));
        assert!(!content.contains("..."));
    }

    #[test]
    fn generate_arc_id_has_correct_format() {
        let id = generate_arc_id();
        assert!(id.starts_with("arc_"));
        assert!(id.len() > 10);
    }

    fn make_entry(entry_type: EntryType, source: &str, content: &str) -> ArcEntry {
        ArcEntry {
            id: 0,
            arc_id: "arc_x".into(),
            entry_type,
            source: source.into(),
            content: content.into(),
            metadata: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            turn_id: None,
        }
    }

    #[test]
    fn build_arc_snippet_picks_last_two_content_entries() {
        let entries = vec![
            make_entry(EntryType::EmailEvent, "email", "First email body"),
            make_entry(EntryType::ToolCall, "agent", "memory_recall"),
            make_entry(EntryType::Message, "user", "Hi there"),
            make_entry(EntryType::Message, "assistant", "Hello back"),
        ];
        let snippet = build_arc_snippet(&entries);
        assert!(
            !snippet.contains("memory_recall"),
            "tool_call entries should be skipped"
        );
        assert!(snippet.contains("user: Hi there"));
        assert!(snippet.contains("assistant: Hello back"));
        assert!(snippet.contains(" | "));
    }

    #[test]
    fn build_arc_snippet_empty_when_no_content_entries() {
        let entries = vec![make_entry(EntryType::ToolCall, "agent", "do_thing")];
        assert_eq!(build_arc_snippet(&entries), "");
        assert_eq!(build_arc_snippet(&[]), "");
    }

    #[test]
    fn build_arc_snippet_truncates_long_content_per_entry_and_overall() {
        // Each entry has 200-char content; per-entry cap is 150, so two
        // entries joined with " | " would be ~305 chars, hitting the
        // overall 300-char cap as well.
        let long_a = "a".repeat(200);
        let long_b = "b".repeat(200);
        let entries = vec![
            make_entry(EntryType::Message, "user", &long_a),
            make_entry(EntryType::Message, "assistant", &long_b),
        ];
        let snippet = build_arc_snippet(&entries);
        // Hits the overall cap.
        assert!(snippet.ends_with("..."));
        assert!(snippet.len() <= 303);
    }

    #[test]
    fn notif_copy_notify_only_preserves_legacy_format() {
        let copy = build_sense_notification_copy(
            false,
            None,
            "email",
            "alice@x.com",
            "Hello",
            "ignored",
            "Body line",
            NotificationUrgency::Medium,
        );
        assert_eq!(copy.title, "email: Hello");
        assert_eq!(copy.body, "Body line");
        assert_eq!(copy.urgency, NotificationUrgency::Medium);
        assert!(!copy.skip_humanize);
    }

    #[test]
    fn notif_copy_silent_approve_says_handling() {
        use athen_core::risk::RiskDecision;
        let copy = build_sense_notification_copy(
            true,
            Some(&RiskDecision::SilentApprove),
            "email",
            "alice@x.com",
            "Hello",
            "reason",
            "ignored",
            NotificationUrgency::Medium,
        );
        assert_eq!(copy.title, "Athen is handling email from alice@x.com");
        assert_eq!(copy.body, "Subject: Hello");
        assert!(copy.skip_humanize);
    }

    #[test]
    fn notif_copy_human_confirm_says_wants_to_act() {
        use athen_core::risk::RiskDecision;
        let copy = build_sense_notification_copy(
            true,
            Some(&RiskDecision::HumanConfirm),
            "message",
            "Bob",
            "Send money?",
            "Risky transfer request",
            "ignored",
            NotificationUrgency::Low,
        );
        assert_eq!(copy.title, "Athen wants to act on message from Bob");
        assert!(copy.body.contains("Approve in app"));
        assert!(copy.body.contains("Risky transfer request"));
        assert_eq!(copy.urgency, NotificationUrgency::High);
        assert!(copy.skip_humanize);
    }

    #[test]
    fn notif_copy_hard_block_announces_block() {
        use athen_core::risk::RiskDecision;
        let copy = build_sense_notification_copy(
            true,
            Some(&RiskDecision::HardBlock),
            "email",
            "spammer",
            "Click here!",
            "Phishing pattern",
            "ignored",
            NotificationUrgency::Low,
        );
        assert_eq!(copy.title, "Athen blocked email from spammer");
        assert_eq!(copy.body, "Phishing pattern");
        assert_eq!(copy.urgency, NotificationUrgency::High);
        assert!(copy.skip_humanize);
    }

    #[test]
    fn notif_copy_dispatch_with_no_decision_falls_back() {
        // Bridge errored: behave like notify-only.
        let copy = build_sense_notification_copy(
            true,
            None,
            "email",
            "alice",
            "Hello",
            "reason",
            "Body line",
            NotificationUrgency::High,
        );
        assert_eq!(copy.title, "email: Hello");
        assert_eq!(copy.body, "Body line");
        assert_eq!(copy.urgency, NotificationUrgency::High);
        assert!(!copy.skip_humanize);
    }

    #[test]
    fn build_arc_snippet_strips_newlines_and_quotes() {
        let entries = vec![make_entry(
            EntryType::Message,
            "user",
            "line one\nline \"two\"",
        )];
        let snippet = build_arc_snippet(&entries);
        assert!(!snippet.contains('\n'));
        assert!(!snippet.contains('"'));
    }

    fn dummy_triage() -> SenseTriage {
        SenseTriage {
            relevance: "medium".into(),
            reason: "test reason".into(),
            suggested_action: "reply".into(),
            target_arc: TriageTarget::NewArc { name: "t".into() },
        }
    }

    // ---------- build_context_message: contact-resolution branches ----------

    #[test]
    fn ctx_email_owner_with_resolved_name_says_from_self() {
        let triage = dummy_triage();
        let body = serde_json::json!({});
        let msg = build_context_message(
            &EventSource::Email,
            "alex@example.com",
            "Hello",
            "body text",
            &body,
            &triage,
            true,
            Some("Alex Albiol"),
        );
        assert!(
            msg.contains("Email from Alex Albiol (you)"),
            "missing owner-with-name greeting: {msg}"
        );
        // Owner-from-name path must NOT use the generic "(the owner)" template.
        assert!(
            !msg.contains("Email from you (the owner)"),
            "fell back to legacy owner template: {msg}"
        );
        // No contact-search instruction when sender is known.
        assert!(
            !msg.contains("check if this sender exists in contacts"),
            "leaked contact-search hint: {msg}"
        );
    }

    #[test]
    fn ctx_email_owner_unresolved_falls_back_to_legacy() {
        let triage = dummy_triage();
        let body = serde_json::json!({});
        let msg = build_context_message(
            &EventSource::Email,
            "alex@example.com",
            "Hello",
            "body text",
            &body,
            &triage,
            true,
            None,
        );
        assert!(msg.contains("Email from you (the owner)"), "{msg}");
    }

    #[test]
    fn ctx_email_non_owner_with_resolved_name_names_contact() {
        let triage = dummy_triage();
        let body = serde_json::json!({});
        let msg = build_context_message(
            &EventSource::Email,
            "bob@example.com",
            "Project update",
            "Hi there",
            &body,
            &triage,
            false,
            Some("Bob Smith"),
        );
        assert!(
            msg.contains("Email from Bob Smith <bob@example.com>"),
            "missing name+email header: {msg}"
        );
        assert!(
            msg.contains("This sender is in your contacts as \"Bob Smith\""),
            "missing known-sender note: {msg}"
        );
        assert!(
            !msg.contains("check if this sender exists in contacts"),
            "leaked contact-search hint: {msg}"
        );
        assert!(
            !msg.contains("Sender identifier:"),
            "leaked raw identifier line: {msg}"
        );
    }

    #[test]
    fn ctx_email_non_owner_unresolved_keeps_legacy_instructions() {
        let triage = dummy_triage();
        let body = serde_json::json!({});
        let msg = build_context_message(
            &EventSource::Email,
            "stranger@example.com",
            "Hello",
            "body",
            &body,
            &triage,
            false,
            None,
        );
        assert!(msg.contains("Email from stranger@example.com"), "{msg}");
        assert!(
            msg.contains("Sender identifier: stranger@example.com (type: Email)"),
            "{msg}"
        );
        assert!(
            msg.contains("check if this sender exists in contacts"),
            "{msg}"
        );
    }

    #[test]
    fn ctx_messaging_owner_with_resolved_name_says_from_self() {
        let triage = dummy_triage();
        let body = serde_json::json!({});
        let msg = build_context_message(
            &EventSource::Messaging,
            "12345",
            "(no subject)",
            "Yo",
            &body,
            &triage,
            true,
            Some("Alex Albiol"),
        );
        assert!(msg.contains("Message from Alex Albiol (you)"), "{msg}");
        assert!(!msg.contains("Message from you (owner)\n\n"), "{msg}");
    }

    #[test]
    fn ctx_messaging_non_owner_with_resolved_name_names_contact() {
        let triage = dummy_triage();
        let body = serde_json::json!({
            "sender_user_id": 98765i64,
            "sender_username": "bobsmith",
            "sender_first_name": "Bob",
        });
        let msg = build_context_message(
            &EventSource::Messaging,
            "98765",
            "(no subject)",
            "Hey",
            &body,
            &triage,
            false,
            Some("Bob Smith"),
        );
        assert!(msg.contains("Message from Bob Smith"), "{msg}");
        // Keeps the Telegram identifier breakdown.
        assert!(msg.contains("Telegram user ID: 98765"), "{msg}");
        assert!(msg.contains("Telegram username: @bobsmith"), "{msg}");
        // Names the contact, drops the search hint.
        assert!(
            msg.contains("This sender is in your contacts as \"Bob Smith\""),
            "{msg}"
        );
        assert!(
            !msg.contains("check if this sender exists in contacts"),
            "{msg}"
        );
    }

    #[test]
    fn ctx_messaging_non_owner_unresolved_keeps_legacy_instructions() {
        let triage = dummy_triage();
        let body = serde_json::json!({
            "sender_user_id": 11111i64,
        });
        let msg = build_context_message(
            &EventSource::Messaging,
            "11111",
            "(no subject)",
            "Hey",
            &body,
            &triage,
            false,
            None,
        );
        assert!(msg.contains("Message from 11111"), "{msg}");
        assert!(msg.contains("Telegram user ID: 11111"), "{msg}");
        assert!(
            msg.contains("check if this sender exists in contacts"),
            "{msg}"
        );
    }

    #[test]
    fn time_window_groups_recent_arcs() {
        let now = chrono::Utc::now();
        let recent = (now - chrono::Duration::seconds(60)).to_rfc3339();
        let old = (now - chrono::Duration::seconds(600)).to_rfc3339();

        let arcs = vec![
            ArcMeta {
                id: "arc_old".into(),
                name: "Old".into(),
                source: ArcSource::Messaging,
                status: ArcStatus::Active,
                parent_arc_id: None,
                merged_into_arc_id: None,
                created_at: old.clone(),
                updated_at: old,
                entry_count: 3,
                primary_reply_channel: None,
                active_profile_id: None,
                summarized_through_entry_id: None,
                pinned_provider_id: None,
                pinned_slug: None,
                reasoning_effort_override: None,
                tier_override: None,
                triage_plan: None,
                user_goal: None,
                user_goal_criteria: None,
                goal_status: None,
                goal_blocked_reason: None,
                plan: None,
                security_mode_override: None,
            },
            ArcMeta {
                id: "arc_recent".into(),
                name: "Recent".into(),
                source: ArcSource::Messaging,
                status: ArcStatus::Active,
                parent_arc_id: None,
                merged_into_arc_id: None,
                created_at: recent.clone(),
                updated_at: recent,
                entry_count: 1,
                primary_reply_channel: None,
                active_profile_id: None,
                summarized_through_entry_id: None,
                pinned_provider_id: None,
                pinned_slug: None,
                reasoning_effort_override: None,
                tier_override: None,
                triage_plan: None,
                user_goal: None,
                user_goal_criteria: None,
                goal_status: None,
                goal_blocked_reason: None,
                plan: None,
                security_mode_override: None,
            },
        ];

        // Within 5 min window → finds recent messaging arc.
        let result = find_recent_arc_from_source(&arcs, &ArcSource::Messaging, 300);
        assert_eq!(result, Some("arc_recent".into()));

        // Different source → no match.
        let result = find_recent_arc_from_source(&arcs, &ArcSource::Email, 300);
        assert_eq!(result, None);

        // Very short window → no match (60s old, 10s window).
        let result = find_recent_arc_from_source(&arcs, &ArcSource::Messaging, 10);
        assert_eq!(result, None);
    }
}
