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

use athen_core::event::{EventSource, SenseEvent};
use athen_core::llm::{
    ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent, ModelProfile,
    Role as LlmRole,
};
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_core::traits::llm::LlmRouter;
use athen_llm::router::DefaultLlmRouter;
use athen_persistence::arcs::{ArcMeta, ArcSource, ArcStatus, ArcStore, EntryType};

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
        format!("{}...", &body_text[..1000])
    } else {
        body_text.clone()
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

    // Step 1: Triage via LLM (with arc context for matching).
    let triage = triage_event(
        router,
        &event.source,
        sender,
        summary,
        &body_for_triage,
        &recent_arcs,
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
        let context_msg = build_context_message(
            &event.source,
            sender,
            summary,
            &body_text,
            &event.content.body,
            &triage,
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

    // Step 4: Emit frontend event.
    let body_preview: String = if body_text.len() > 500 {
        format!("{}...", &body_text[..500])
    } else {
        body_text.trim().to_string()
    };

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
        }),
    );

    // Step 5: Notify through orchestrator channels.
    if let Some(notifier) = notifier {
        let urgency = match triage.relevance.as_str() {
            "high" => NotificationUrgency::High,
            "medium" => NotificationUrgency::Medium,
            _ => NotificationUrgency::Low,
        };

        let title = format!("{}: {}", source_name, summary);
        let body_notif = if body_preview.len() > 200 {
            format!("{}...", &body_preview[..200])
        } else {
            body_preview.clone()
        };

        let notification = Notification {
            id: Uuid::new_v4(),
            urgency,
            title,
            body: body_notif,
            origin: NotificationOrigin::SenseRouter,
            arc_id: Some(arc_id.clone()),
            task_id: None,
            created_at: Utc::now(),
            requires_response: false,
        };

        notifier.notify(notification).await;
    }

    true
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

/// Build a system context message that gives the agent full awareness of
/// the sense event when the user opens this Arc.
fn build_context_message(
    source: &EventSource,
    sender: &str,
    subject: &str,
    body_text: &str,
    body_json: &serde_json::Value,
    triage: &SenseTriage,
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
        }
        EventSource::Email => {
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
        EventSource::Messaging => {
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
#[allow(clippy::too_many_arguments)]
async fn route_new_arc_to_profile(
    arc_store: Option<&ArcStore>,
    profile_store: Option<&Arc<athen_persistence::profiles::SqliteProfileStore>>,
    profile_embedder: &Arc<dyn athen_core::traits::embedding::EmbeddingProvider>,
    profile_embedding_cache: &crate::state::ProfileEmbeddingCache,
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

    let Some(decision) = pick_profile_blended(
        &classified,
        &profiles,
        query_embedding.as_deref(),
        &profile_embeddings,
    ) else {
        info!(
            arc = arc_id,
            source = source,
            domain = ?classified.domain,
            kind = ?classified.kind,
            "Profile router: no positive match; leaving arc on default"
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
        format!("{}...", &body[..500])
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
async fn triage_event(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    source: &EventSource,
    sender: &str,
    subject: &str,
    body: &str,
    recent_arcs: &[ArcMeta],
) -> SenseTriage {
    let source_name = source_display_name(source);

    // Build the existing arcs context for the prompt.
    let arcs_context = if recent_arcs.is_empty() {
        String::new()
    } else {
        let mut ctx = String::from("\n\nExisting active Arcs (conversations/threads):\n");
        for arc in recent_arcs {
            let source_label = arc.source.as_str();
            ctx.push_str(&format!(
                "- ID: \"{}\" | Name: \"{}\" | Source: {} | Entries: {}\n",
                arc.id, arc.name, source_label, arc.entry_count,
            ));
        }
        ctx
    };

    let arc_matching_instruction = if recent_arcs.is_empty() {
        r#"For "arc_name": give a short, descriptive name summarizing the topic (max 40 chars, e.g. "Meeting with John", "Server alert")."#.to_string()
    } else {
        r#"IMPORTANT — Arc matching:
Look at the existing Arcs listed above. If this message is CLEARLY related to one of them (same topic, same person, same thread, a reply to an ongoing conversation), set "existing_arc_id" to that arc's ID.
Only create a new arc ("arc_name") if the message is about a genuinely new topic not covered by any existing arc.
When in doubt, prefer creating a new arc over merging into the wrong one.

Set EITHER "existing_arc_id" OR "arc_name", never both."#.to_string()
    };

    let prompt = format!(
        r#"You are a personal assistant triaging an incoming {source_name}.

From: {sender}
Subject: {subject}
Body:
{body}{arcs_context}
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

#[cfg(test)]
mod tests {
    use super::*;

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
