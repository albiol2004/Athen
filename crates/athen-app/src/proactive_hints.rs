//! Proactive help hints — rules engine + background checker.
//!
//! Evaluates lightweight config-state rules and surfaces one-liner nudges
//! via the in-app notification channel. Rate-limited to 1 hint per hour,
//! permanently dismissable per hint ID.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use crate::ui_bridge::UiBridge;
use tokio::sync::Mutex;
use tracing::{debug, info};
use uuid::Uuid;

use athen_core::config::{AthenConfig, EmbeddingMode};
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_persistence::hint_dismissals::HintDismissalStore;

// ---------------------------------------------------------------------------
// Hint definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ProactiveHint {
    pub hint_id: String,
    pub title: String,
    pub body: String,
    /// Settings panel to navigate to (e.g. "calendar", "email", "providers").
    pub action_panel: Option<String>,
    /// athen_docs topic slug for the agent to load.
    pub skill_topic: Option<String>,
}

// ---------------------------------------------------------------------------
// Context snapshot (pure data, no store references)
// ---------------------------------------------------------------------------

pub struct HintContext {
    pub config: AthenConfig,
    pub calendar_source_count: usize,
    #[allow(dead_code)]
    pub active_provider_id: String,
    pub is_local_provider: bool,
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

fn check_no_calendar_source(ctx: &HintContext) -> Option<ProactiveHint> {
    if ctx.calendar_source_count > 0 {
        return None;
    }
    Some(ProactiveHint {
        hint_id: "no_calendar_source".into(),
        title: "Connect your calendar".into(),
        body: "Link iCloud, Google, or another calendar so Athen can help with scheduling and reminders.".into(),
        action_panel: Some("calendar-sources".into()),
        skill_topic: Some("setup-calendar-source".into()),
    })
}

fn check_no_email(ctx: &HintContext) -> Option<ProactiveHint> {
    if ctx.config.email.enabled {
        return None;
    }
    Some(ProactiveHint {
        hint_id: "no_email".into(),
        title: "Connect your email".into(),
        body: "Set up IMAP so Athen can monitor incoming mail and send replies on your behalf."
            .into(),
        action_panel: Some("email".into()),
        skill_topic: Some("setup-email".into()),
    })
}

fn check_no_search_key(ctx: &HintContext) -> Option<ProactiveHint> {
    let brave = ctx.config.web_search.brave_api_key.trim();
    let tavily = ctx.config.web_search.tavily_api_key.trim();
    if !brave.is_empty() || !tavily.is_empty() {
        return None;
    }
    Some(ProactiveHint {
        hint_id: "no_search_key".into(),
        title: "Better web search available".into(),
        body: "Add a Brave or Tavily API key for higher-quality search results. Brave offers 2,000 free queries/month.".into(),
        action_panel: Some("cloud-apis".into()),
        skill_topic: Some("setup-cloud-api-endpoint".into()),
    })
}

fn check_no_telegram(ctx: &HintContext) -> Option<ProactiveHint> {
    if ctx.config.telegram.enabled {
        return None;
    }
    Some(ProactiveHint {
        hint_id: "no_telegram".into(),
        title: "Get notifications on your phone".into(),
        body: "Connect a Telegram bot so Athen can reach you when you're away from the app.".into(),
        action_panel: Some("telegram".into()),
        skill_topic: None,
    })
}

fn check_embedding_off(ctx: &HintContext) -> Option<ProactiveHint> {
    if ctx.config.embeddings.mode != EmbeddingMode::Off {
        return None;
    }
    Some(ProactiveHint {
        hint_id: "embedding_off".into(),
        title: "Enable memory".into(),
        body:
            "Turn on embeddings so Athen remembers past conversations and recalls relevant context."
                .into(),
        action_panel: Some("embedding".into()),
        skill_topic: None,
    })
}

fn check_local_no_family(ctx: &HintContext) -> Option<ProactiveHint> {
    if !ctx.is_local_provider {
        return None;
    }
    // We can't easily check model_family from config alone — the quirks
    // registry lives in athen-llm. For now, always emit this for local
    // providers as a gentle reminder. The user dismisses permanently once
    // they've set it.
    Some(ProactiveHint {
        hint_id: "local_no_family".into(),
        title: "Set your model family".into(),
        body: "Local models work better when you set the model family in Settings → Bundles. This enables proper tool-call parsing.".into(),
        action_panel: Some("bundles".into()),
        skill_topic: Some("pick-local-model".into()),
    })
}

type RuleFn = fn(&HintContext) -> Option<ProactiveHint>;

const RULES: &[RuleFn] = &[
    check_no_calendar_source,
    check_no_email,
    check_no_search_key,
    check_no_telegram,
    check_embedding_off,
    check_local_no_family,
];

pub fn evaluate_rules(ctx: &HintContext) -> Vec<ProactiveHint> {
    RULES.iter().filter_map(|rule| rule(ctx)).collect()
}

// ---------------------------------------------------------------------------
// Background checker
// ---------------------------------------------------------------------------

pub struct ProactiveHintChecker {
    store: HintDismissalStore,
    last_emitted: Mutex<Option<Instant>>,
}

impl ProactiveHintChecker {
    pub fn new(store: HintDismissalStore) -> Self {
        Self {
            store,
            last_emitted: Mutex::new(None),
        }
    }

    pub async fn check_and_emit(
        &self,
        ctx: HintContext,
        app_handle: &UiBridge,
        notifier: Option<&Arc<crate::notifier::NotificationOrchestrator>>,
    ) {
        let permanent = match self.store.list_permanent().await {
            Ok(p) => p.into_iter().collect::<HashSet<_>>(),
            Err(e) => {
                debug!(error = %e, "Failed to load hint dismissals");
                return;
            }
        };

        let candidates = evaluate_rules(&ctx);
        let actionable: Vec<_> = candidates
            .into_iter()
            .filter(|h| !permanent.contains(&h.hint_id))
            .collect();

        if actionable.is_empty() {
            debug!("No actionable proactive hints");
            return;
        }

        // Rate limit: 1 hint per hour.
        {
            let mut last = self.last_emitted.lock().await;
            if let Some(ts) = *last {
                if ts.elapsed() < Duration::from_secs(3600) {
                    debug!("Rate-limited: skipping proactive hint");
                    return;
                }
            }
            *last = Some(Instant::now());
        }

        // Emit the first actionable hint only.
        let hint = &actionable[0];
        info!(hint_id = %hint.hint_id, "Emitting proactive hint");

        // Emit as a Tauri event so the frontend renders a hint card.
        app_handle.emit("proactive-hint", hint);

        // Also deliver through the notifier for Telegram-away delivery.
        if let Some(notifier) = notifier {
            let notification = Notification {
                id: Uuid::new_v4(),
                urgency: NotificationUrgency::Low,
                title: hint.title.clone(),
                body: hint.body.clone(),
                origin: NotificationOrigin::System,
                arc_id: None,
                task_id: None,
                created_at: Utc::now(),
                requires_response: false,
                skip_humanize: true,
                body_long: None,
            };
            notifier.notify(notification).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::config::EmbeddingMode;

    fn default_ctx() -> HintContext {
        HintContext {
            config: AthenConfig::default(),
            calendar_source_count: 0,
            active_provider_id: "deepseek".into(),
            is_local_provider: false,
        }
    }

    #[test]
    fn all_hints_fire_on_default_config() {
        let ctx = default_ctx();
        let hints = evaluate_rules(&ctx);
        let ids: Vec<_> = hints.iter().map(|h| h.hint_id.as_str()).collect();
        assert!(ids.contains(&"no_calendar_source"));
        assert!(ids.contains(&"no_email"));
        assert!(ids.contains(&"no_search_key"));
        assert!(ids.contains(&"no_telegram"));
        // embedding default is Automatic, not Off — so no hint
        assert!(!ids.contains(&"embedding_off"));
        // not local provider
        assert!(!ids.contains(&"local_no_family"));
    }

    #[test]
    fn no_hints_when_everything_configured() {
        let mut config = AthenConfig::default();
        config.email.enabled = true;
        config.telegram.enabled = true;
        config.web_search.brave_api_key = "test-key".into();
        let ctx = HintContext {
            config,
            calendar_source_count: 3,
            active_provider_id: "deepseek".into(),
            is_local_provider: false,
        };
        let hints = evaluate_rules(&ctx);
        assert!(hints.is_empty());
    }

    #[test]
    fn local_provider_triggers_family_hint() {
        let ctx = HintContext {
            config: AthenConfig::default(),
            calendar_source_count: 5,
            active_provider_id: "ollama".into(),
            is_local_provider: true,
        };
        let hints = evaluate_rules(&ctx);
        let ids: Vec<_> = hints.iter().map(|h| h.hint_id.as_str()).collect();
        assert!(ids.contains(&"local_no_family"));
    }

    #[test]
    fn embedding_off_triggers_hint() {
        let mut config = AthenConfig::default();
        config.embeddings.mode = EmbeddingMode::Off;
        let ctx = HintContext {
            config,
            calendar_source_count: 0,
            active_provider_id: "deepseek".into(),
            is_local_provider: false,
        };
        let hints = evaluate_rules(&ctx);
        let ids: Vec<_> = hints.iter().map(|h| h.hint_id.as_str()).collect();
        assert!(ids.contains(&"embedding_off"));
    }

    #[test]
    fn search_key_present_suppresses_hint() {
        let mut config = AthenConfig::default();
        config.web_search.tavily_api_key = "tvly-abc123".into();
        let ctx = HintContext {
            config,
            calendar_source_count: 0,
            active_provider_id: "deepseek".into(),
            is_local_provider: false,
        };
        let hints = evaluate_rules(&ctx);
        let ids: Vec<_> = hints.iter().map(|h| h.hint_id.as_str()).collect();
        assert!(!ids.contains(&"no_search_key"));
    }

    #[test]
    fn hint_has_skill_topic_or_panel() {
        let ctx = default_ctx();
        let hints = evaluate_rules(&ctx);
        for h in &hints {
            assert!(
                h.action_panel.is_some() || h.skill_topic.is_some(),
                "Hint {} has no action",
                h.hint_id
            );
        }
    }
}
