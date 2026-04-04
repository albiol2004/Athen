//! Generic sense-to-arc router.
//!
//! Processes incoming `SenseEvent`s from any sense monitor (email, calendar,
//! messaging, etc.) by triaging their relevance via LLM, then creating or
//! merging into an Arc.

use std::sync::Arc;

use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;
use tracing::{info, warn};

use athen_core::event::{EventSource, SenseEvent};
use athen_core::llm::{
    ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent,
    ModelProfile, Role as LlmRole,
};
use athen_core::traits::llm::LlmRouter;
use athen_llm::router::DefaultLlmRouter;
use athen_persistence::arcs::{ArcSource, ArcStore, EntryType};

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
#[allow(dead_code)]
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
pub async fn process_sense_event(
    event: &SenseEvent,
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    arc_store: &Option<ArcStore>,
    app_handle: &AppHandle,
) -> bool {
    let source_name = source_display_name(&event.source);
    let summary = event.content.summary.as_deref().unwrap_or("(no subject)");
    let sender = event.sender.as_ref()
        .map(|s| s.display_name.as_deref().unwrap_or(&s.identifier))
        .unwrap_or("unknown");

    let body_text = event.content.body.get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    // Truncate body for LLM triage (save tokens).
    let body_for_triage: String = if body_text.len() > 1000 {
        format!("{}...", &body_text[..1000])
    } else {
        body_text.to_string()
    };

    // Step 1: Triage via LLM.
    let triage = triage_event(
        router, &event.source, sender, summary, &body_for_triage,
    ).await;

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
    let arc_source = event_source_to_arc_source(&event.source);
    let arc_id = match &triage.target_arc {
        TriageTarget::NewArc { name } => {
            let id = generate_arc_id();
            if let Some(store) = arc_store {
                if let Err(e) = store.create_arc(&id, name, arc_source).await {
                    warn!("Failed to create arc for sense event: {e}");
                }
            }
            id
        }
        TriageTarget::ExistingArc { arc_id } => arc_id.clone(),
    };

    // Step 3: Persist as ArcEntry.
    let entry_type = event_source_to_entry_type(&event.source);
    let entry_content = format_entry_content(sender, summary, body_text);
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
        if let Err(e) = store.add_entry(
            &arc_id, entry_type, source_name, &entry_content, Some(entry_metadata.clone()),
        ).await {
            warn!("Failed to persist sense event entry: {e}");
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

    true
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn source_display_name(source: &EventSource) -> &'static str {
    match source {
        EventSource::Email => "email",
        EventSource::Calendar => "calendar",
        EventSource::Messaging => "message",
        EventSource::UserInput => "user_input",
        EventSource::System => "system",
    }
}

fn event_source_to_arc_source(source: &EventSource) -> ArcSource {
    match source {
        EventSource::Email => ArcSource::Email,
        EventSource::Calendar => ArcSource::Calendar,
        EventSource::Messaging => ArcSource::Messaging,
        EventSource::UserInput => ArcSource::UserInput,
        EventSource::System => ArcSource::System,
    }
}

fn event_source_to_entry_type(source: &EventSource) -> EntryType {
    match source {
        EventSource::Email => EntryType::EmailEvent,
        EventSource::Calendar => EntryType::CalendarEvent,
        EventSource::Messaging => EntryType::Message,
        EventSource::UserInput => EntryType::Message,
        EventSource::System => EntryType::SystemEvent,
    }
}

fn generate_arc_id() -> String {
    chrono::Utc::now().format("arc_%Y%m%d_%H%M%S").to_string()
}

fn format_entry_content(sender: &str, subject: &str, body: &str) -> String {
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
async fn triage_event(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    source: &EventSource,
    sender: &str,
    subject: &str,
    body: &str,
) -> SenseTriage {
    let source_name = source_display_name(source);

    let prompt = format!(
        r#"You are a personal assistant triaging an incoming {source_name}.

From: {sender}
Subject: {subject}
Body:
{body}

Respond with ONLY a JSON object (no markdown, no explanation):
{{
  "relevance": "ignore|low|medium|high",
  "reason": "one-line explanation",
  "suggested_action": "none|read|reply|calendar|urgent",
  "arc_name": "short descriptive name for this thread (max 40 chars)"
}}

Classification rules:
- "ignore": ONLY for obvious machine-generated spam, marketing newsletters, automated CI/CD notifications, promotional bulk email
- "low": mailing list digests, non-urgent automated updates, social media notifications
- "medium": any message from a real person, work-related, personal messages, requests, questions, invitations
- "high": urgent requests, deadlines, time-sensitive matters, security alerts

IMPORTANT: Default to "medium" unless you are very confident it is spam or automated. Real messages from real people should NEVER be classified as "ignore".

For "arc_name": give a short, descriptive name summarizing the topic (e.g. "Meeting with John", "Server alert", "Project proposal from Alice")."#
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
fn parse_triage_response(text: &str) -> SenseTriage {
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
            let relevance = v.get("relevance")
                .and_then(|r| r.as_str())
                .unwrap_or("medium")
                .to_string();
            let reason = v.get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("No reason provided")
                .to_string();
            let suggested_action = v.get("suggested_action")
                .and_then(|r| r.as_str())
                .unwrap_or("read")
                .to_string();
            let arc_name = v.get("arc_name")
                .and_then(|r| r.as_str())
                .unwrap_or("Incoming event")
                .to_string();

            SenseTriage {
                relevance,
                reason,
                suggested_action,
                target_arc: TriageTarget::NewArc { name: arc_name },
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
