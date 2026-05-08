//! `LlmArcCompactor` — Phase-1 implementation of the [`ArcCompactor`] port.
//!
//! Drives an LLM through the §4 prompt in `docs/ARC_COMPACTION.md` to
//! collapse an arc's prefix into a structured summary. Burst detection is
//! the heuristic from §2: a contiguous run of `tool_call` entries between
//! two `message` entries is one burst that closes when the next assistant
//! message is emitted. Token estimation is `chars / 4`.
//!
//! Phase-2+ swaps for entropy/embedding-based scorers slot in as
//! alternative `ArcCompactor` implementations behind the same trait — see
//! `docs/ARC_COMPACTION.md` §7.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use athen_core::error::{AthenError, Result};
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::traits::compaction::{
    ArcCompactor, ArcContextView, CompactionOutcome, ContextEntry,
};
use athen_core::traits::llm::LlmRouter;
use athen_persistence::arcs::{ArcEntry, ArcStore, EntryType};

/// Fallback budgets used only when no provider config can be resolved
/// (e.g. tests, broken config). 128k window with §5's 65/30 split, which
/// is conservative for every common cloud provider and won't fire eagerly
/// even on small local models — those should be configured properly via
/// `ProviderConfig`. Resolution happens through `resolve_compaction_budget`.
pub const FALLBACK_CONTEXT_WINDOW_TOKENS: u32 = 128_000;
pub const FALLBACK_COMPACTION_TRIGGER_PCT: u8 = 65;
pub const FALLBACK_COMPACTION_TARGET_PCT: u8 = 30;

/// Resolve the per-arc compaction budget from the active provider's
/// settings, falling back to the conservative defaults when the provider
/// is missing (legacy boot, fresh install, or test fixture).
///
/// Returns `(trigger_tokens, target_tokens)` already converted from the
/// per-provider `compaction_trigger_pct` / `compaction_target_pct` and
/// `context_window_tokens`. The two values are derived from the SAME
/// active provider so trigger and target are guaranteed to come from one
/// coherent policy — never mixed across providers.
///
/// Trigger is clamped above target so the hysteresis invariant
/// (`trigger > target`) holds even if a user configures a profile where
/// trigger_pct <= target_pct. Without that clamp, `should_compact` would
/// fire above the target, `compact` would no-op (already under target),
/// and we'd ping-pong every turn.
pub fn resolve_compaction_budget(
    config: &athen_core::config::AthenConfig,
    active_provider_id: &str,
) -> (u32, u32) {
    let (window, trigger_pct, target_pct) = config
        .models
        .providers
        .get(active_provider_id)
        .map(|p| {
            (
                p.context_window_tokens,
                p.compaction_trigger_pct,
                p.compaction_target_pct,
            )
        })
        .unwrap_or((
            FALLBACK_CONTEXT_WINDOW_TOKENS,
            FALLBACK_COMPACTION_TRIGGER_PCT,
            FALLBACK_COMPACTION_TARGET_PCT,
        ));

    let target = (window as u64 * target_pct as u64 / 100) as u32;
    let trigger_raw = (window as u64 * trigger_pct as u64 / 100) as u32;
    let trigger = trigger_raw.max(target.saturating_add(1));
    (trigger, target)
}

/// Resolve the active provider's sampling temperature override. Returns
/// `None` if no provider entry exists or the user has not set a value —
/// the agent builder treats that as "let the provider adapter pick its
/// baked-in default" (currently 0.7 across the OpenAI-compat / DeepSeek
/// paths). Lives next to `resolve_compaction_budget` so every per-task
/// resolver call site reads from the same provider entry without
/// re-fetching the config.
pub fn resolve_provider_temperature(
    config: &athen_core::config::AthenConfig,
    active_provider_id: &str,
) -> Option<f32> {
    config
        .models
        .providers
        .get(active_provider_id)
        .and_then(|p| p.temperature)
}

/// Convert an `ArcContextView` into the inputs the executor consumes.
///
/// Returns `(messages, system_suffix)`:
/// - `messages` — verbatim tail history with `source → role` mapping.
///   Non-message tail entries (tool_call rows, system_event, etc.) are
///   dropped because the executor's `context_messages` is a chat history,
///   not a raw audit log.
/// - `system_suffix` — compaction summary and most-recent-tool-result
///   cache, packaged as text intended to be appended to the leading
///   system message (via `AgentBuilder::external_system_suffix`). These
///   used to ride as mid-stream `Role::System` messages, but strict chat
///   templates (Qwen, Llama) raise on system roles past position 0, so
///   they now live inside the single leading system message.
pub fn view_to_messages(view: &ArcContextView) -> (Vec<ChatMessage>, String) {
    let mut suffix = String::new();
    if let Some(ref s) = view.summary {
        suffix.push_str(&format!(
            "<<COMPACTION SUMMARY — covers earlier turns of this arc; \
             the verbatim history below picks up after this point>>\n{}\n\n",
            s.content
        ));
    }
    if !view.tool_cache.is_empty() {
        suffix.push_str("<<LATEST TOOL RESULTS — most recent successful call per tool>>\n");
        for e in &view.tool_cache {
            suffix.push_str("- ");
            suffix.push_str(&e.content.replace('\n', " "));
            suffix.push('\n');
        }
        suffix.push('\n');
    }

    let mut tail = Vec::new();
    for e in &view.tail {
        if e.entry_type != "message" {
            continue;
        }
        let role = match e.source.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            "tool" => Role::Tool,
            _ => continue,
        };
        tail.push(ChatMessage {
            role,
            content: MessageContent::Text(e.content.clone()),
        });
    }
    (tail, suffix)
}

/// Token estimation: `chars / 4`. Stable upper-bound estimator suitable
/// for trigger decisions; per-provider tokenizers (Phase-3) would refine
/// the number without changing the trigger contract.
fn estimate_tokens(s: &str) -> u32 {
    (s.chars().count() / 4) as u32
}

fn entry_token_estimate(e: &ArcEntry) -> u32 {
    estimate_tokens(&e.content) + estimate_tokens(&e.source) + 4
}

fn to_context_entry(e: &ArcEntry) -> ContextEntry {
    ContextEntry {
        id: e.id,
        source: e.source.clone(),
        content: e.content.clone(),
        entry_type: e.entry_type.as_str().to_string(),
    }
}

/// LLM-driven compactor. Constructs a single LLM call per compaction
/// using the §4 fixed-category prompt; never paraphrases user
/// constraints (the prompt forbids it).
#[derive(Clone)]
pub struct LlmArcCompactor {
    arc_store: ArcStore,
    router: Arc<RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
}

impl LlmArcCompactor {
    pub fn new(
        arc_store: ArcStore,
        router: Arc<RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    ) -> Self {
        Self { arc_store, router }
    }

    /// Build the §4 summarization prompt over `entries`. Entries are
    /// rendered in id-ascending order with role tags so the LLM can
    /// recover structure without us pre-grouping into bursts (the LLM is
    /// asked to identify open actions and pending approvals from the
    /// raw stream).
    fn build_summary_prompt(entries: &[ArcEntry]) -> String {
        let mut body = String::with_capacity(entries.len() * 80);
        for e in entries {
            let kind = e.entry_type.as_str();
            let tag = match (kind, e.source.as_str()) {
                ("message", "user") => "USER".to_string(),
                ("message", "assistant") => "ASSISTANT".to_string(),
                ("message", "system") => "SYSTEM".to_string(),
                ("message", other) => format!("MESSAGE/{other}"),
                ("tool_call", _) => "TOOL_CALL".to_string(),
                ("email_event", _) => "EMAIL".to_string(),
                ("calendar_event", _) => "CALENDAR".to_string(),
                ("system_event", _) => "SYS_EVENT".to_string(),
                ("summary", _) => "PRIOR_SUMMARY".to_string(),
                (other, _) => other.to_uppercase(),
            };
            let trimmed = e.content.replace('\n', " ");
            body.push_str(&format!("[{tag}] {trimmed}\n"));
        }
        body
    }

    /// The static header. Hard-coded categories per §4 — the LLM does
    /// not freelance the structure. The "do not paraphrase user-stated
    /// constraints or decisions" clause is load-bearing.
    fn summary_system_prompt() -> &'static str {
        "You are compacting an arc of work for an autonomous assistant. \
Produce a structured summary the assistant can act from on its next turn. \
DO NOT paraphrase direct user quotes about constraints or decisions — \
preserve them verbatim. DO NOT invent details not present in the input. \
If failed-then-succeeded patterns exist for a tool, preserve the failure \
mode (e.g. \"failed twice with X, succeeded after Y\").

Output exactly these sections, in this order:

ARC GOAL: <what the user/sense set this arc up to accomplish>
PARTICIPANTS: <contacts, threads, channels involved>
DECISIONS: <non-obvious choices the agent or user committed to>
CONSTRAINTS: <user-stated rules, verbatim>
PENDING APPROVALS: <anything currently waiting on the user, or 'none'>
TOOL OUTCOMES: <one line per named tool series; preserve failure→success patterns>
OPEN ACTION: <what the agent was about to do next, verbatim from the latest entries; or 'none'>"
    }
}

#[async_trait]
impl ArcCompactor for LlmArcCompactor {
    async fn should_compact(&self, arc_id: &str, trigger_tokens: u32) -> Result<bool> {
        let entries = self.arc_store.load_entries(arc_id).await?;
        let total: u32 = entries.iter().map(entry_token_estimate).sum();
        Ok(total > trigger_tokens)
    }

    async fn compact(&self, arc_id: &str, target_tokens: u32) -> Result<CompactionOutcome> {
        let prior_summary = self.arc_store.load_latest_summary(arc_id).await?;
        let cutoff = prior_summary.as_ref().map(|s| s.id).unwrap_or(0);
        let entries = self.arc_store.load_entries_after(arc_id, cutoff).await?;
        if entries.is_empty() {
            return Ok(CompactionOutcome {
                compacted: false,
                summarized_through_entry_id: None,
                tokens_before: 0,
                tokens_after: 0,
            });
        }
        let total: u32 = entries.iter().map(entry_token_estimate).sum();
        if total <= target_tokens {
            return Ok(CompactionOutcome {
                compacted: false,
                summarized_through_entry_id: None,
                tokens_before: total,
                tokens_after: total,
            });
        }

        // Keep the last 25% of entries as verbatim tail; summarize the
        // rest. This is a per-arc heuristic, not per-burst — burst-aware
        // grouping lands in Phase-2 (`burst_id` column). We never
        // summarize a single trailing entry: if there are <4 entries past
        // the prior summary, no compaction happens at all.
        if entries.len() < 4 {
            return Ok(CompactionOutcome {
                compacted: false,
                summarized_through_entry_id: None,
                tokens_before: total,
                tokens_after: total,
            });
        }
        let split = (entries.len() * 3) / 4;
        let to_summarize = &entries[..split];
        let summarized_through = to_summarize
            .last()
            .map(|e| e.id)
            .ok_or_else(|| AthenError::Other("empty summarize range".into()))?;

        let mut prompt_body = String::new();
        if let Some(prev) = prior_summary.as_ref() {
            prompt_body.push_str("[PRIOR_SUMMARY]\n");
            prompt_body.push_str(&prev.content);
            prompt_body.push_str("\n\n[NEW ENTRIES SINCE PRIOR SUMMARY]\n");
        }
        prompt_body.push_str(&Self::build_summary_prompt(to_summarize));

        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(prompt_body),
            }],
            max_tokens: Some(2048),
            temperature: Some(0.0),
            tools: None,
            system_prompt: Some(Self::summary_system_prompt().to_string()),
        };

        let router = self.router.read().await.clone();
        let response = router.route(&request).await.map_err(|e| {
            AthenError::Other(format!("Compaction LLM call failed for arc {arc_id}: {e}"))
        })?;
        let summary_text = response.content.trim();
        if summary_text.is_empty() {
            return Err(AthenError::Other(format!(
                "Compaction LLM returned empty summary for arc {arc_id}"
            )));
        }

        let metadata = serde_json::json!({
            "summarized_entries": to_summarize.len(),
            "tokens_before": total,
            "covers_through_id": summarized_through,
        });
        self.arc_store
            .compact_arc(arc_id, summary_text, Some(metadata), summarized_through)
            .await?;

        let summary_tokens = estimate_tokens(summary_text);
        let tail_tokens: u32 = entries[split..].iter().map(entry_token_estimate).sum();
        Ok(CompactionOutcome {
            compacted: true,
            summarized_through_entry_id: Some(summarized_through),
            tokens_before: total,
            tokens_after: summary_tokens + tail_tokens,
        })
    }

    async fn load_context_view(&self, arc_id: &str) -> Result<ArcContextView> {
        // The arc's `summarized_through_entry_id` is authoritative for
        // "what the summary covers." The summary entry itself has an id
        // strictly greater than the cutoff (it was inserted after the
        // covered entries), so using `summary.id` as the cutoff would
        // wrongly exclude entries the summary does NOT cover but which
        // landed between the cutoff and the summary write. Always read
        // the cutoff from `ArcMeta`.
        let meta = self.arc_store.get_arc(arc_id).await?;
        let cutoff = meta.as_ref().and_then(|m| m.summarized_through_entry_id);
        let summary = if cutoff.is_some() {
            self.arc_store.load_latest_summary(arc_id).await?
        } else {
            None
        };
        let after = cutoff.unwrap_or(0);
        let raw_tail = self.arc_store.load_entries_after(arc_id, after).await?;
        let tail: Vec<ContextEntry> = raw_tail
            .iter()
            .filter(|e| e.entry_type != EntryType::Summary)
            .map(to_context_entry)
            .collect();

        // Tool-series cache: latest successful tool_call per distinct
        // tool name, drawn from the COMPACTED prefix (entries with
        // id <= cutoff). Without this, the agent re-loads historical
        // tool state by re-running the tool. The latest call is enough
        // because earlier calls are subsumed by the summary.
        let tool_cache: Vec<ContextEntry> = if let Some(c) = cutoff {
            self.build_tool_cache(arc_id, c).await?
        } else {
            Vec::new()
        };

        Ok(ArcContextView {
            summary: summary.as_ref().map(to_context_entry),
            tail,
            tool_cache,
        })
    }
}

impl LlmArcCompactor {
    /// Build the latest-per-tool-series cache from entries with id <=
    /// `cutoff_id`. Tool name is read from the entry's metadata when
    /// present; entries without a recoverable name are skipped.
    async fn build_tool_cache(&self, arc_id: &str, cutoff_id: i64) -> Result<Vec<ContextEntry>> {
        let all = self.arc_store.load_entries(arc_id).await?;
        let mut latest: std::collections::BTreeMap<String, ArcEntry> =
            std::collections::BTreeMap::new();
        for e in all
            .into_iter()
            .filter(|e| e.id <= cutoff_id && e.entry_type == EntryType::ToolCall)
        {
            let tool_name = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("tool"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(name) = tool_name {
                latest.insert(name, e);
            }
        }
        Ok(latest.values().map(to_context_entry).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex as TMutex;

    async fn empty_store() -> ArcStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let store = ArcStore::new(StdArc::new(TMutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    #[test]
    fn estimate_tokens_chars_div_four() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    fn mk_provider_for_test(window: u32, trig: u8, tgt: u8) -> athen_core::config::ProviderConfig {
        athen_core::config::ProviderConfig {
            auth: athen_core::config::AuthType::None,
            default_model: "test".into(),
            endpoint: None,
            context_window_tokens: window,
            compaction_trigger_pct: trig,
            compaction_target_pct: tgt,
            supports_vision: false,
            supports_documents: false,
            family: athen_core::llm::ModelFamily::Default,
            temperature: None,
        }
    }

    /// Resolver returns the per-provider tokens with hysteresis preserved.
    #[test]
    fn resolve_compaction_budget_uses_active_provider() {
        use athen_core::config::AthenConfig;
        let mut cfg = AthenConfig::default();
        cfg.models
            .providers
            .insert("qwen-local".into(), mk_provider_for_test(32_000, 70, 25));
        let (trigger, target) = resolve_compaction_budget(&cfg, "qwen-local");
        assert_eq!(target, 8_000);
        assert_eq!(trigger, 22_400);
        assert!(trigger > target);
    }

    /// Unknown provider id falls back to the conservative defaults rather
    /// than 0/0 (which would compact every turn) or panicking.
    #[test]
    fn resolve_compaction_budget_falls_back_for_unknown_provider() {
        use athen_core::config::AthenConfig;
        let cfg = AthenConfig::default();
        let (trigger, target) = resolve_compaction_budget(&cfg, "no-such-provider");
        let expected_trigger =
            FALLBACK_CONTEXT_WINDOW_TOKENS as u64 * FALLBACK_COMPACTION_TRIGGER_PCT as u64 / 100;
        let expected_target =
            FALLBACK_CONTEXT_WINDOW_TOKENS as u64 * FALLBACK_COMPACTION_TARGET_PCT as u64 / 100;
        assert_eq!(trigger as u64, expected_trigger);
        assert_eq!(target as u64, expected_target);
    }

    /// Temperature resolver returns the active provider's override and
    /// falls through to `None` when the field is unset or the provider
    /// is unknown — `None` is the agent builder's signal to use the
    /// adapter's baked-in default.
    #[test]
    fn resolve_provider_temperature_reads_active_override_or_returns_none() {
        use athen_core::config::AthenConfig;
        let mut cfg = AthenConfig::default();
        let mut p_set = mk_provider_for_test(32_000, 65, 30);
        p_set.temperature = Some(0.2);
        cfg.models.providers.insert("set".into(), p_set);
        cfg.models
            .providers
            .insert("unset".into(), mk_provider_for_test(32_000, 65, 30));

        assert_eq!(resolve_provider_temperature(&cfg, "set"), Some(0.2));
        assert_eq!(resolve_provider_temperature(&cfg, "unset"), None);
        assert_eq!(resolve_provider_temperature(&cfg, "no-such-provider"), None);
    }

    /// Hysteresis invariant: even a misconfigured provider where
    /// trigger_pct <= target_pct must yield trigger > target so the
    /// should_compact / compact pair doesn't ping-pong every turn.
    #[test]
    fn resolve_compaction_budget_clamps_trigger_above_target() {
        use athen_core::config::AthenConfig;
        let mut cfg = AthenConfig::default();
        cfg.models
            .providers
            .insert("broken".into(), mk_provider_for_test(10_000, 20, 50));
        let (trigger, target) = resolve_compaction_budget(&cfg, "broken");
        assert_eq!(target, 5_000);
        assert!(
            trigger > target,
            "trigger ({trigger}) must exceed target ({target})"
        );
    }

    #[tokio::test]
    async fn build_summary_prompt_tags_roles_and_strips_newlines() {
        let store = empty_store().await;
        store
            .create_arc("a", "A", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("a", EntryType::Message, "user", "hello\nworld", None, None)
            .await
            .unwrap();
        store
            .add_entry("a", EntryType::ToolCall, "agent", "ran X", None, None)
            .await
            .unwrap();
        store
            .add_entry("a", EntryType::Message, "assistant", "did X", None, None)
            .await
            .unwrap();
        let entries = store.load_entries("a").await.unwrap();
        let prompt = LlmArcCompactor::build_summary_prompt(&entries);
        assert!(prompt.contains("[USER] hello world"));
        assert!(prompt.contains("[TOOL_CALL] ran X"));
        assert!(prompt.contains("[ASSISTANT] did X"));
        assert!(prompt.contains('\n'));
    }

    #[tokio::test]
    async fn load_context_view_with_no_summary_returns_all_tail() {
        // Build a compactor with an unused router stub — load_context_view
        // does not call the LLM. We construct via Arc<RwLock<Arc<...>>>
        // matching the AppState plumbing; the inner DefaultLlmRouter is
        // fine to leave unused for this read-only path.
        let store = empty_store().await;
        store
            .create_arc("a", "A", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        for i in 0..3 {
            store
                .add_entry(
                    "a",
                    EntryType::Message,
                    "user",
                    &format!("turn {i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        // Build a compactor with a default router. We don't exercise it.
        let router = athen_llm::router::DefaultLlmRouter::new(
            Default::default(),
            Default::default(),
            athen_llm::budget::BudgetTracker::new(None),
        );
        let router = StdArc::new(tokio::sync::RwLock::new(StdArc::new(router)));
        let compactor = LlmArcCompactor::new(store.clone(), router);

        let view = compactor.load_context_view("a").await.unwrap();
        assert!(view.summary.is_none());
        assert_eq!(view.tail.len(), 3);
        assert!(view.tool_cache.is_empty());
    }

    #[tokio::test]
    async fn load_context_view_with_summary_returns_summary_plus_tail() {
        let store = empty_store().await;
        store
            .create_arc("a", "A", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        for i in 0..4 {
            store
                .add_entry(
                    "a",
                    EntryType::Message,
                    "user",
                    &format!("e{i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let entries = store.load_entries("a").await.unwrap();
        let cutoff = entries[1].id;
        store
            .compact_arc("a", "summary text", None, cutoff)
            .await
            .unwrap();
        // Add one more entry after the summary.
        store
            .add_entry("a", EntryType::Message, "user", "e4", None, None)
            .await
            .unwrap();

        let router = athen_llm::router::DefaultLlmRouter::new(
            Default::default(),
            Default::default(),
            athen_llm::budget::BudgetTracker::new(None),
        );
        let router = StdArc::new(tokio::sync::RwLock::new(StdArc::new(router)));
        let compactor = LlmArcCompactor::new(store.clone(), router);

        let view = compactor.load_context_view("a").await.unwrap();
        let summary = view.summary.expect("summary present");
        assert_eq!(summary.entry_type, "summary");
        assert_eq!(summary.content, "summary text");
        // Tail = entries with id > summary.id, excluding summary entries
        // themselves: original e2, e3, e4.
        assert_eq!(view.tail.len(), 3);
        assert_eq!(view.tail[0].content, "e2");
        assert_eq!(view.tail[2].content, "e4");
    }
}
