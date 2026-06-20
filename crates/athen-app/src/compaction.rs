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
use athen_core::risk::TriagePlan;
use athen_core::traits::compaction::{
    ArcCompactor, ArcContextView, CompactionOutcome, ContextEntry,
};
use athen_core::traits::llm::LlmRouter;
use athen_persistence::arcs::{ArcEntry, ArcStore, EntryType};

/// Tool names whose successful invocation produces side effects the
/// compactor MUST preserve verbatim in the summary's TOOL OUTCOMES
/// section. A summary that silently drops "we sent an email" is worse
/// than one that drops a noisy read result — the user can't be told a
/// reply happened, the agent will re-send, etc. Used by
/// `LlmArcCompactor::collect_write_provenance` to mark entries that get
/// hoisted into a "DO NOT DROP" block in the compaction prompt.
const WRITE_PROVENANCE_TOOLS: &[&str] = &[
    "email_send",
    "send_telegram",
    "write",
    "edit",
    "shell_execute",
    "http_request",
    "memory_store",
    "create_wakeup",
    "calendar_event_create",
    "calendar_event_update",
    "calendar_event_delete",
    "contact_upsert",
];

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
///   Includes prior `tool_call` rows, rendered as one-line assistant
///   audit prose (see [`to_context_entry`]) at their chronological
///   position. Without this, a provider failover or any mid-arc error
///   left the next provider with prose-only history — the agent denied
///   its own actions, re-ran finished work, and confabulated when asked
///   to recall. Other non-message entries (system_event, etc.) are
///   still dropped.
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
        // Source="system" Message entries are app-authored one-shot
        // notices (e.g. the post-rewind hint). They ride as Role::User
        // wrapped in `<system-reminder>` framing — strict chat templates
        // (Qwen / Llama) reject Role::System past position 0, and the
        // framing tags are the Claude-Code-style convention models react
        // to anyway. Appending these at the tail keeps the prompt
        // cache-friendly: existing cached prefix is untouched and the
        // notice itself folds into the cache after the next turn.
        let (role, content) = match (e.entry_type.as_str(), e.source.as_str()) {
            ("message", "user") => (Role::User, e.content.clone()),
            ("message", "assistant") => (Role::Assistant, e.content.clone()),
            ("message", "system") => (
                Role::User,
                format!(
                    "<system-reminder>\n{}\n</system-reminder>",
                    e.content.trim()
                ),
            ),
            ("message", "tool") => (Role::Tool, e.content.clone()),
            // Persisted tool_call rows replay as assistant-side audit
            // prose at their chronological position. The content was
            // already flattened in `to_context_entry` so we just push it.
            ("tool_call", _) => (Role::Assistant, e.content.clone()),
            _ => continue,
        };
        tail.push(ChatMessage {
            role,
            content: MessageContent::Text(content),
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
    // `tool_call` rows are persisted with only the tool name in `content`
    // and the rich `{tool, args, result, error, status}` payload in
    // `metadata`. `ContextEntry` has no metadata field (it's intentionally
    // lean — athen-core stays decoupled from the persistence enum), so
    // when we convert tool_call rows for downstream consumption we flatten
    // the metadata into the content string here. That way every caller —
    // the rebuilt chat history, the tool_cache rendering, anyone else —
    // sees a single readable line instead of a bare tool name.
    //
    // Source="system" Message rows (the post-rewind hint and friends)
    // carry parallel summaries: a user-facing line in `content` and an
    // agent-facing line in `metadata.llm_hint`. The chat UI shows
    // `content` directly; here, when building the LLM view, we swap in
    // `llm_hint` if present so the model gets the version that addresses
    // it as the agent.
    let content = if e.entry_type == EntryType::ToolCall {
        render_tool_call_audit(e)
    } else if e.entry_type == EntryType::Message && e.source == "system" {
        e.metadata
            .as_ref()
            .and_then(|m| m.get("llm_hint"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| e.content.clone())
    } else {
        e.content.clone()
    };
    ContextEntry {
        id: e.id,
        source: e.source.clone(),
        content,
        entry_type: e.entry_type.as_str().to_string(),
    }
}

/// Render a persisted `tool_call` ArcEntry as a single line of audit
/// prose. Long results are truncated so replay doesn't balloon the prompt
/// (a 50 KB shell stdout has no business living in `context_messages`
/// turn after turn). The previously-correct tool/args/result is the
/// minimum needed for the next turn — for the full text the agent can
/// re-run the tool or read the arc UI.
fn render_tool_call_audit(e: &ArcEntry) -> String {
    let Some(meta) = e.metadata.as_ref() else {
        return format!("[tool={} (no metadata)]", e.content);
    };
    let tool = meta
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or(&e.content);
    let status = meta.get("status").and_then(|v| v.as_str()).unwrap_or("?");

    let args = meta
        .get("args")
        .map(|v| one_line(v, 240))
        .unwrap_or_default();
    let result = meta
        .get("result")
        .map(|v| one_line(v, 600))
        .unwrap_or_default();
    let error = meta
        .get("error")
        .map(|v| one_line(v, 400))
        .unwrap_or_default();

    let mut out = format!("[tool={tool} status={status}");
    if !args.is_empty() {
        out.push_str(" args=");
        out.push_str(&args);
    }
    if !result.is_empty() {
        out.push_str(" → result=");
        out.push_str(&result);
    }
    if !error.is_empty() {
        out.push_str(" → error=");
        out.push_str(&error);
    }
    out.push(']');
    out
}

/// Stringify a JSON value into a single line capped at `limit` chars.
/// Null returns empty so callers can skip empty fields cleanly.
fn one_line(v: &serde_json::Value, limit: usize) -> String {
    let s = match v {
        serde_json::Value::Null => return String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    let s = s.trim();
    if s.chars().count() > limit {
        let mut t: String = s.chars().take(limit).collect();
        t.push_str(" …(truncated)");
        t
    } else {
        s.to_string()
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

    /// Build the §4 summarization prompt over `entries`, optionally
    /// prefixed by a MISSION anchor (from the arc's persisted
    /// `triage_plan`) and a WRITE-PROVENANCE block listing entries
    /// whose tool name appears in `WRITE_PROVENANCE_TOOLS`. The anchors
    /// are rendered with explicit "preserve verbatim" / "do not drop"
    /// markers so the summarizer (which is told in the system prompt to
    /// honour them) keeps them intact. Entries are rendered in
    /// id-ascending order with role tags so the LLM can recover
    /// structure without us pre-grouping into bursts.
    fn build_summary_prompt(
        entries: &[ArcEntry],
        triage_plan: Option<&TriagePlan>,
        write_entries: &[&ArcEntry],
    ) -> String {
        let mut body = String::with_capacity(entries.len() * 80 + 512);
        if let Some(plan) = triage_plan {
            let acceptance = plan.acceptance_criteria.trim();
            let scope = plan.scope.trim();
            if !acceptance.is_empty() || !scope.is_empty() {
                body.push_str("[MISSION — preserve VERBATIM in DECISIONS or CONSTRAINTS]\n");
                if !acceptance.is_empty() {
                    body.push_str("ACCEPTANCE CRITERION: ");
                    body.push_str(&acceptance.replace('\n', " "));
                    body.push('\n');
                }
                if !scope.is_empty() {
                    body.push_str("SCOPE GUARDRAIL: ");
                    body.push_str(&scope.replace('\n', " "));
                    body.push('\n');
                }
                body.push('\n');
            }
        }
        if !write_entries.is_empty() {
            body.push_str(
                "[WRITE-PROVENANCE ENTRIES — DO NOT DROP. Each MUST be reflected in TOOL OUTCOMES with the tool name and a short description of what was sent/changed.]\n",
            );
            for e in write_entries {
                let tool_name = e
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool_call");
                let trimmed = e.content.replace('\n', " ");
                body.push_str(&format!("[WRITE/{tool_name} id={}] {trimmed}\n", e.id));
            }
            body.push('\n');
        }
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

    /// Return references to entries whose `metadata.tool` matches one of
    /// `WRITE_PROVENANCE_TOOLS`. Order-preserving so the prompt lists
    /// them in chronological order. Entries with no `tool` metadata are
    /// skipped — the compactor cannot infer write-vs-read for opaque
    /// rows. Limited to a small cap so a pathological burst doesn't
    /// dominate the prompt; the LLM only needs the protect-list as
    /// guidance, not exhaustive enumeration.
    fn collect_write_provenance(entries: &[ArcEntry]) -> Vec<&ArcEntry> {
        const MAX_WRITE_ENTRIES: usize = 24;
        let mut out: Vec<&ArcEntry> = Vec::new();
        for e in entries {
            if e.entry_type != EntryType::ToolCall {
                continue;
            }
            let tool = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("tool"))
                .and_then(|v| v.as_str());
            if let Some(name) = tool {
                if WRITE_PROVENANCE_TOOLS.contains(&name) {
                    out.push(e);
                    if out.len() >= MAX_WRITE_ENTRIES {
                        break;
                    }
                }
            }
        }
        out
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

If the input contains a [MISSION] block, copy its ACCEPTANCE CRITERION verbatim \
into DECISIONS (or CONSTRAINTS if it reads as a rule), and its SCOPE GUARDRAIL \
verbatim into CONSTRAINTS. These define what 'done' means for the task — the \
agent reads them next turn to decide whether to keep going.

If the input contains a [WRITE-PROVENANCE ENTRIES] block, every listed entry \
MUST be reflected in TOOL OUTCOMES with the tool name plus a one-line description \
of what was sent or changed (recipient + subject for email, chat_id + message \
gist for telegram, path for write/edit, endpoint + method gist for http_request). \
A summary that silently drops a send/write event is a critical bug — the agent \
will re-send.

Output exactly these sections, in this order:

ARC GOAL: <what the user/sense set this arc up to accomplish>
PARTICIPANTS: <contacts, threads, channels involved>
DECISIONS: <non-obvious choices the agent or user committed to>
CONSTRAINTS: <user-stated rules, verbatim>
PENDING APPROVALS: <anything currently waiting on the user, or 'none'>
TOOL OUTCOMES: <one line per named tool series; preserve failure→success patterns AND every WRITE-PROVENANCE entry>
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

        // Load the arc's persisted TriagePlan (Slice 1-3) so the
        // summarizer keeps the acceptance criterion + scope verbatim, and
        // identify write-bearing entries (Slice 6 protect-list) so the
        // summarizer can't silently drop "we sent the email" / "we wrote
        // the file" rows. Both are best-effort: a missing plan or empty
        // provenance just means the prompt skips that block — never a
        // hard failure on the compaction path.
        let triage_plan = self
            .arc_store
            .get_arc(arc_id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.triage_plan);
        let write_entries = Self::collect_write_provenance(to_summarize);

        let mut prompt_body = String::new();
        if let Some(prev) = prior_summary.as_ref() {
            prompt_body.push_str("[PRIOR_SUMMARY]\n");
            prompt_body.push_str(&prev.content);
            prompt_body.push_str("\n\n[NEW ENTRIES SINCE PRIOR SUMMARY]\n");
        }
        prompt_body.push_str(&Self::build_summary_prompt(
            to_summarize,
            triage_plan.as_ref(),
            &write_entries,
        ));

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
            reasoning_effort: athen_core::llm::ReasoningEffort::default(),
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

/// Minimum number of new arc entries (past the project's fold watermark for
/// that arc) required before we spend an LLM call folding the arc into the
/// project summary. Mirrors `LlmArcCompactor`'s "< 4 entries → skip"
/// heuristic so trivial arc switches (a single ack, a quick "thanks") cost
/// zero tokens.
const PROJECT_FOLD_MIN_DELTA_ENTRIES: i64 = 4;

/// LLM-driven project-wide compactor.
///
/// Maintains a single durable cross-conversation summary per Project by
/// incrementally folding the *delta* of a just-left arc into the existing
/// summary. The fold is cheap by construction: it operates on the arc's
/// already-existing compaction summary plus the small tail of entries past
/// the project's per-arc watermark — never the raw transcript — and only
/// fires once enough new entries have accrued (`PROJECT_FOLD_MIN_DELTA_ENTRIES`).
///
/// This is the per-switch step of the incremental hierarchical compaction
/// described in `docs/PROJECTS.md` §"project summary". Call-site wiring
/// (firing on arc-switch) lives in a separate slice — this type only exposes
/// the fold primitive.
#[derive(Clone)]
pub struct LlmProjectCompactor {
    arc_store: ArcStore,
    project_store: Arc<athen_persistence::projects::ProjectStore>,
    router: Arc<RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
}

impl LlmProjectCompactor {
    pub fn new(
        arc_store: ArcStore,
        project_store: Arc<athen_persistence::projects::ProjectStore>,
        router: Arc<RwLock<Arc<athen_llm::router::DefaultLlmRouter>>>,
    ) -> Self {
        Self {
            arc_store,
            project_store,
            router,
        }
    }

    /// The static header for the project-fold call. Instructs the model to
    /// fold a new delta into the existing durable summary while preserving
    /// the load-bearing facts a future arc would need.
    fn project_summary_system_prompt() -> &'static str {
        "You maintain a concise, durable cross-conversation summary of a \
Project. Fold the new delta into the existing summary. Preserve decisions, \
facts about the user, deliverables, and open threads. Stay terse; do not \
repeat boilerplate. Output only the updated summary."
    }

    /// Render the delta text for the just-left arc: its already-existing
    /// compaction summary (if any) followed by the entries past the
    /// project's watermark for this arc, each as one compact `source: content`
    /// line with long contents truncated. Folding summaries-plus-tail (not the
    /// raw transcript) keeps the call cheap.
    fn build_delta_text(arc_summary: Option<&str>, tail: &[ArcEntry]) -> String {
        let mut out = String::with_capacity(tail.len() * 80 + 256);
        if let Some(s) = arc_summary {
            let s = s.trim();
            if !s.is_empty() {
                out.push_str("ARC SUMMARY SO FAR:\n");
                out.push_str(s);
                out.push_str("\n\n");
            }
        }
        if !tail.is_empty() {
            out.push_str("RECENT ENTRIES:\n");
            for e in tail {
                let content = one_line(&serde_json::Value::String(e.content.clone()), 400);
                if content.is_empty() {
                    continue;
                }
                out.push_str("- ");
                out.push_str(&e.source);
                out.push_str(": ");
                out.push_str(&content);
                out.push('\n');
            }
        }
        out
    }

    /// Fold the just-left arc's delta into the project summary. Incremental
    /// and best-effort. Returns `Ok(false)` if the min-delta gate skipped it
    /// (no LLM call), `Ok(true)` if the summary was updated.
    pub async fn fold_arc_into_project(&self, project_id: &str, arc_id: &str) -> Result<bool> {
        // 1. Determine the arc's latest entry id. `load_entries_after(.., 0)`
        //    returns every entry in id-ascending order; the last is the max id.
        let all = self.arc_store.load_entries_after(arc_id, 0).await?;
        let Some(latest_id) = all.last().map(|e| e.id) else {
            // Arc has no entries — nothing to fold.
            return Ok(false);
        };

        // 2. Where did we last fold up to for this (project, arc)?
        let watermark = self
            .project_store
            .get_fold_watermark(project_id, arc_id)
            .await?
            .unwrap_or(0);

        // 3. Min-delta gate — bail before any LLM call if too little is new.
        if latest_id - watermark < PROJECT_FOLD_MIN_DELTA_ENTRIES {
            return Ok(false);
        }

        // 4. Build the delta from the arc's existing compaction summary plus
        //    the entries past the watermark (cheap — summaries, not transcript).
        let arc_summary = self.arc_store.load_latest_summary(arc_id).await?;
        let tail = self.arc_store.load_entries_after(arc_id, watermark).await?;
        let delta = Self::build_delta_text(arc_summary.as_ref().map(|s| s.content.as_str()), &tail);

        // 5. Current project summary, if any.
        let existing_summary = self
            .project_store
            .get_project(project_id)
            .await?
            .and_then(|p| p.summary);

        // 6. Assemble the fold prompt.
        let mut prompt_body = String::new();
        prompt_body.push_str("[PROJECT_SUMMARY]\n");
        prompt_body.push_str(existing_summary.as_deref().unwrap_or("(none yet)"));
        prompt_body.push_str("\n\n[DELTA FROM ARC ");
        prompt_body.push_str(arc_id);
        prompt_body.push_str("]\n");
        prompt_body.push_str(&delta);

        // 7. Cheap-tier LLM call — mirrors `LlmArcCompactor::compact`.
        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(prompt_body),
            }],
            max_tokens: Some(2048),
            temperature: Some(0.0),
            tools: None,
            system_prompt: Some(Self::project_summary_system_prompt().to_string()),
            reasoning_effort: athen_core::llm::ReasoningEffort::default(),
        };

        let router = self.router.read().await.clone();
        let response = router.route(&request).await.map_err(|e| {
            AthenError::Other(format!(
                "Project fold LLM call failed for project {project_id} arc {arc_id}: {e}"
            ))
        })?;
        let summary_text = response.content.trim();
        if summary_text.is_empty() {
            return Err(AthenError::Other(format!(
                "Project fold LLM returned empty summary for project {project_id} arc {arc_id}"
            )));
        }

        // 8. Persist the updated summary and advance the watermark.
        self.project_store
            .set_summary(project_id, summary_text)
            .await?;
        self.project_store
            .set_fold_watermark(project_id, arc_id, latest_id)
            .await?;

        // 9. Done — summary updated.
        Ok(true)
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

    /// Build an `ArcStore` + `ProjectStore` sharing one in-memory connection,
    /// so the arc's entries and the project's fold state live in the same DB.
    async fn arc_and_project_stores(
    ) -> (ArcStore, StdArc<athen_persistence::projects::ProjectStore>) {
        let conn = StdArc::new(TMutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        let arc_store = ArcStore::new(conn.clone());
        arc_store.init_schema().await.unwrap();
        let project_store = athen_persistence::projects::ProjectStore::new(conn);
        project_store.init_schema().await.unwrap();
        (arc_store, StdArc::new(project_store))
    }

    fn stub_router() -> Arc<RwLock<Arc<athen_llm::router::DefaultLlmRouter>>> {
        let router = athen_llm::router::DefaultLlmRouter::new(
            Default::default(),
            Default::default(),
            athen_llm::budget::BudgetTracker::new(None),
        );
        StdArc::new(tokio::sync::RwLock::new(StdArc::new(router)))
    }

    /// The min-delta gate: an arc with fewer than
    /// `PROJECT_FOLD_MIN_DELTA_ENTRIES` new entries past the watermark must
    /// skip the fold entirely — return `Ok(false)`, make no LLM call, and
    /// leave the project summary untouched. The router stub here has no
    /// providers, so if the gate failed to short-circuit, `route` would error
    /// and the test would fail loudly.
    #[tokio::test]
    async fn fold_skips_below_min_delta() {
        let (arc_store, project_store) = arc_and_project_stores().await;
        let project = project_store.create_project("Proj", None).await.unwrap();
        arc_store
            .create_arc(
                "arc1",
                "Arc 1",
                athen_persistence::arcs::ArcSource::UserInput,
            )
            .await
            .unwrap();
        // Three entries (< MIN_DELTA of 4) past the implicit watermark of 0.
        for i in 0..3 {
            arc_store
                .add_entry(
                    "arc1",
                    EntryType::Message,
                    "user",
                    &format!("m{i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let compactor =
            LlmProjectCompactor::new(arc_store.clone(), project_store.clone(), stub_router());
        let folded = compactor
            .fold_arc_into_project(&project.id, "arc1")
            .await
            .unwrap();

        assert!(!folded, "fold must be skipped below the min-delta gate");
        // Summary stays None — no LLM call, no write.
        let got = project_store
            .get_project(&project.id)
            .await
            .unwrap()
            .unwrap();
        assert!(got.summary.is_none(), "summary must remain unset");
        // Watermark untouched.
        assert!(project_store
            .get_fold_watermark(&project.id, "arc1")
            .await
            .unwrap()
            .is_none());
    }

    /// An arc with no entries returns `Ok(false)` without touching the router
    /// or the project summary.
    #[tokio::test]
    async fn fold_skips_empty_arc() {
        let (arc_store, project_store) = arc_and_project_stores().await;
        let project = project_store.create_project("Proj", None).await.unwrap();
        arc_store
            .create_arc(
                "empty",
                "Empty",
                athen_persistence::arcs::ArcSource::UserInput,
            )
            .await
            .unwrap();

        let compactor =
            LlmProjectCompactor::new(arc_store.clone(), project_store.clone(), stub_router());
        let folded = compactor
            .fold_arc_into_project(&project.id, "empty")
            .await
            .unwrap();
        assert!(!folded);
        let got = project_store
            .get_project(&project.id)
            .await
            .unwrap()
            .unwrap();
        assert!(got.summary.is_none());
    }

    /// The fold prompt assembles the delta from the arc's existing summary plus
    /// its recent entries, one compact `source: content` line each, with long
    /// contents truncated. This exercises the cheap "fold summaries, not raw
    /// transcript" path without needing a live router.
    #[test]
    fn build_delta_text_renders_summary_and_tail() {
        let now = chrono::Utc::now().to_rfc3339();
        let store_entries = vec![
            ArcEntry {
                id: 5,
                arc_id: "arc1".into(),
                entry_type: EntryType::Message,
                source: "user".into(),
                content: "do the thing".into(),
                metadata: None,
                created_at: now.clone(),
                turn_id: None,
            },
            ArcEntry {
                id: 6,
                arc_id: "arc1".into(),
                entry_type: EntryType::Message,
                source: "assistant".into(),
                content: "x".repeat(1000),
                metadata: None,
                created_at: now,
                turn_id: None,
            },
        ];
        let delta =
            LlmProjectCompactor::build_delta_text(Some("prior arc summary"), &store_entries);
        assert!(delta.contains("ARC SUMMARY SO FAR:"));
        assert!(delta.contains("prior arc summary"));
        assert!(delta.contains("RECENT ENTRIES:"));
        assert!(delta.contains("- user: do the thing"));
        // Long content is truncated (400-char cap + ellipsis marker).
        assert!(delta.contains("…(truncated)"), "got: {delta}");
    }

    /// The project-fold system prompt carries the load-bearing instructions:
    /// fold the delta in, preserve durable facts, output only the summary.
    #[test]
    fn project_summary_system_prompt_has_fold_contract() {
        let sys = LlmProjectCompactor::project_summary_system_prompt();
        assert!(sys.contains("durable"));
        assert!(sys.contains("Fold the new delta"));
        assert!(sys.contains("Output only the updated summary"));
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
            tier_models: std::collections::HashMap::new(),
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
        let prompt = LlmArcCompactor::build_summary_prompt(&entries, None, &[]);
        assert!(prompt.contains("[USER] hello world"));
        assert!(prompt.contains("[TOOL_CALL] ran X"));
        assert!(prompt.contains("[ASSISTANT] did X"));
        assert!(prompt.contains('\n'));
        // No plan / no provenance → no anchor blocks.
        assert!(!prompt.contains("[MISSION"));
        assert!(!prompt.contains("[WRITE-PROVENANCE"));
    }

    #[tokio::test]
    async fn build_summary_prompt_prepends_mission_block_when_plan_present() {
        let store = empty_store().await;
        store
            .create_arc("p", "P", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("p", EntryType::Message, "user", "hi", None, None)
            .await
            .unwrap();
        let entries = store.load_entries("p").await.unwrap();
        let plan = TriagePlan {
            acceptance_criteria: "Reply once with Q3 terms confirmed.".to_string(),
            scope: "NOT a multi-message thread.".to_string(),
        };
        let prompt = LlmArcCompactor::build_summary_prompt(&entries, Some(&plan), &[]);
        assert!(
            prompt.contains("[MISSION — preserve VERBATIM"),
            "missing MISSION anchor: {prompt}"
        );
        assert!(prompt.contains("ACCEPTANCE CRITERION: Reply once with Q3 terms confirmed."));
        assert!(prompt.contains("SCOPE GUARDRAIL: NOT a multi-message thread."));
        // MISSION block precedes the regular entries.
        let mission_pos = prompt.find("[MISSION").unwrap();
        let entry_pos = prompt.find("[USER] hi").unwrap();
        assert!(mission_pos < entry_pos);
    }

    #[tokio::test]
    async fn build_summary_prompt_includes_write_provenance_block() {
        let store = empty_store().await;
        store
            .create_arc("w", "W", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry(
                "w",
                EntryType::ToolCall,
                "agent",
                "sent to user@example.com (subj: hi)",
                Some(serde_json::json!({"tool": "email_send"})),
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "w",
                EntryType::ToolCall,
                "agent",
                "listed /tmp",
                Some(serde_json::json!({"tool": "list_directory"})),
                None,
            )
            .await
            .unwrap();
        let entries = store.load_entries("w").await.unwrap();
        let writes = LlmArcCompactor::collect_write_provenance(&entries);
        assert_eq!(writes.len(), 1, "only email_send is write-bearing");
        let prompt = LlmArcCompactor::build_summary_prompt(&entries, None, &writes);
        assert!(prompt.contains("[WRITE-PROVENANCE ENTRIES — DO NOT DROP."));
        assert!(prompt.contains("[WRITE/email_send id="));
        assert!(prompt.contains("sent to user@example.com (subj: hi)"));
        // The list_directory tool call must NOT be in the protect-list.
        assert!(!prompt.contains("[WRITE/list_directory"));
    }

    #[tokio::test]
    async fn collect_write_provenance_caps_at_max_entries() {
        let store = empty_store().await;
        store
            .create_arc("c", "C", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        for i in 0..40 {
            store
                .add_entry(
                    "c",
                    EntryType::ToolCall,
                    "agent",
                    &format!("write #{i}"),
                    Some(serde_json::json!({"tool": "write"})),
                    None,
                )
                .await
                .unwrap();
        }
        let entries = store.load_entries("c").await.unwrap();
        let writes = LlmArcCompactor::collect_write_provenance(&entries);
        assert_eq!(writes.len(), 24, "must cap at MAX_WRITE_ENTRIES = 24");
    }

    #[tokio::test]
    async fn build_summary_prompt_skips_empty_plan_fields() {
        let store = empty_store().await;
        store
            .create_arc("e", "E", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("e", EntryType::Message, "user", "x", None, None)
            .await
            .unwrap();
        let entries = store.load_entries("e").await.unwrap();
        // Empty/whitespace plan → no MISSION block.
        let plan = TriagePlan {
            acceptance_criteria: "   ".to_string(),
            scope: "\n".to_string(),
        };
        let prompt = LlmArcCompactor::build_summary_prompt(&entries, Some(&plan), &[]);
        assert!(!prompt.contains("[MISSION"));
    }

    #[test]
    fn system_prompt_references_mission_and_write_provenance_rules() {
        let sys = LlmArcCompactor::summary_system_prompt();
        assert!(sys.contains("[MISSION] block"));
        assert!(sys.contains("[WRITE-PROVENANCE ENTRIES] block"));
        assert!(sys.contains("ACCEPTANCE CRITERION"));
        assert!(sys.contains("SCOPE GUARDRAIL"));
        assert!(sys.contains("re-send"));
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

    /// Regression: a Gemini→DeepSeek failover left the new provider with
    /// prose-only history because `view_to_messages` dropped every entry
    /// that wasn't `entry_type == "message"`. Tool calls are persisted to
    /// the arc with full metadata, but rebuilding `context_messages` for
    /// the next turn (or the next provider) discarded them. The agent
    /// then redid finished work, denied its own actions, and confabulated
    /// when asked to recall. This test confirms tool_call rows now ride
    /// through rehydration as assistant-role audit prose.
    #[tokio::test]
    async fn view_to_messages_preserves_tool_calls_across_rehydration() {
        let store = empty_store().await;
        store
            .create_arc("a", "A", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry(
                "a",
                EntryType::Message,
                "user",
                "Write an HTML page and host it on port 7681",
                None,
                None,
            )
            .await
            .unwrap();
        // Tool call burst from the prior (failed-over) turn.
        store
            .add_entry(
                "a",
                EntryType::ToolCall,
                "assistant",
                "write",
                Some(serde_json::json!({
                    "tool": "write",
                    "args": {"path": "/tmp/x/index.html", "size_bytes": 900},
                    "result": "wrote 900B",
                    "error": null,
                    "status": "Completed",
                })),
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a",
                EntryType::ToolCall,
                "assistant",
                "shell_spawn",
                Some(serde_json::json!({
                    "tool": "shell_spawn",
                    "args": {"command": "python3 -m http.server 7681"},
                    "result": {"pid": 12345, "alive": true},
                    "error": null,
                    "status": "Completed",
                })),
                None,
            )
            .await
            .unwrap();
        // Final assistant reply that was overwritten with the rate-limit
        // error when the LLM call failed mid-iteration.
        store
            .add_entry(
                "a",
                EntryType::Message,
                "assistant",
                "Something went wrong: rate limited.",
                None,
                None,
            )
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
        let (messages, _suffix) = view_to_messages(&view);

        // 1 user + 2 tool_call (replayed as assistant) + 1 assistant reply.
        assert_eq!(messages.len(), 4, "messages: {messages:#?}");
        assert!(matches!(messages[0].role, Role::User));
        assert!(matches!(messages[1].role, Role::Assistant));
        assert!(matches!(messages[2].role, Role::Assistant));
        assert!(matches!(messages[3].role, Role::Assistant));
        // Tool-call audit prose must surface the tool name + result so the
        // next provider can see what already happened.
        let MessageContent::Text(ref m1) = messages[1].content else {
            panic!("expected text");
        };
        assert!(m1.contains("tool=write"), "got: {m1}");
        assert!(m1.contains("status=Completed"), "got: {m1}");
        let MessageContent::Text(ref m2) = messages[2].content else {
            panic!("expected text");
        };
        assert!(m2.contains("tool=shell_spawn"), "got: {m2}");
        assert!(m2.contains("python3"), "got: {m2}");
        assert!(m2.contains("\"alive\":true"), "got: {m2}");
    }

    /// source="system" Message entries (used by the post-rewind hint)
    /// must ride as Role::User wrapped in `<system-reminder>` framing.
    /// Mid-stream Role::System breaks strict chat templates (Qwen/Llama),
    /// and the framing tags are the convention models react to anyway.
    #[tokio::test]
    async fn view_to_messages_wraps_system_source_message_as_user_reminder() {
        let store = empty_store().await;
        store
            .create_arc("a", "A", athen_persistence::arcs::ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("a", EntryType::Message, "user", "first turn", None, None)
            .await
            .unwrap();
        // user-facing in content, agent-facing in metadata.llm_hint.
        // The LLM view must substitute the agent-facing variant.
        store
            .add_entry(
                "a",
                EntryType::Message,
                "system",
                "Reverted the most recent change. Files restored: foo.rs.",
                Some(serde_json::json!({
                    "llm_hint": "Out-of-band notice — the user just reverted your most recent change. Re-read foo.rs before referring to it."
                })),
                None,
            )
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
        let (messages, _suffix) = view_to_messages(&view);

        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[1].role, Role::User));
        let MessageContent::Text(ref body) = messages[1].content else {
            panic!("expected text");
        };
        assert!(body.starts_with("<system-reminder>"), "got: {body}");
        assert!(body.ends_with("</system-reminder>"), "got: {body}");
        // The LLM-facing variant from metadata, not the bare user-facing
        // content — so the agent sees the "Re-read foo.rs" instruction.
        assert!(body.contains("Re-read foo.rs"), "got: {body}");
        assert!(body.contains("Out-of-band"), "got: {body}");
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
