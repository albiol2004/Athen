# Arc Compaction

> **Status (2026-05-19): Phase-1 shipped.** The `ArcCompactor` trait, the
> `LlmArcCompactor` implementation, summary persistence, per-provider
> budgets, settings UI, and executor integration are all live. Phase-2/3
> items (explicit `burst_id`, entropy pre-pass, embedding salience,
> hierarchical re-compaction, post-compaction verification turn) remain
> open. See §12 for a per-section implementation map.

Arcs are durable conversation/work threads persisted in `arc_entries` (see
`crates/athen-persistence/src/arcs.rs`). Before Phase-1, every executor dispatch
loaded the **entire** arc into the LLM context. That broke once a single arc
accumulated dozens of turns plus tool-call bursts — the prompt blew past the
model's context window, costs scaled linearly with arc age, and latency
degraded.

This document describes how Athen compacts arcs without losing the
information the agent will actually reach for on its next turn. The body
below is the canonical design; §12 reports what is currently implemented.

## 1. The principle: the "Continue" test

> If I compact this arc and the user's next word is just "Continue", which
> bytes will the agent's hand reach for?

That is the operational definition of salience. Not "is this important in
the abstract" but "would the next dispatch *use* this byte?" The principle
replaces a fuzzy importance score with a test that can be applied per entry.

**Always hot** — must survive compaction verbatim:

- The arc's *current open action* — the in-progress draft (email body,
  pending calendar reschedule, unanswered approval question).
- *Latest outbound state* per channel — last thing Athen said to whom.
- Constraints the user stated directly ("don't email after 8pm", "always
  confirm before sending to legal").
- The latest result of each *active* tool series — once an email is drafted
  and unsent, the body is hot; once sent, it collapses.

**Cold** — summarize aggressively or drop:

- Completed tool-call bursts (a series of `read_file` against the same path
  collapses to "scanned `foo.rs`"; a successful `email_send` collapses to
  "replied to Alex re: project update ✓").
- Retries that eventually succeeded — keep the success, but **preserve a
  one-line failure trace** ("failed twice with `ECONNREFUSED`, succeeded
  after retrying with TLS"). The failure pattern is signal, not noise: it
  tells the next turn "don't repeat X."
- Search/recall results that didn't end up referenced — drop entirely.
- Pleasantries, acknowledgements, status pings.

## 2. Compaction unit: the burst, not the entry

A *burst* is a contiguous sequence of entries serving one sub-goal. While
the sub-goal is open, the burst stays verbatim. When the sub-goal closes
(email sent, file written, search answered), the entire burst collapses to
one summary line — outcome + outbound artifact + (if applicable) failure
trace.

Compacting at the burst level rather than per-entry is the highest-leverage
choice in this design:

- It preserves the failure → success learning pattern automatically.
- It eliminates "dozens of `read_file` against the same path" without any
  per-entry heuristic.
- It pushes the "is this still hot?" decision to a structural moment
  (sub-goal completion) rather than a recurring score-recompute.

**Phase-1 burst detection** — heuristic, no schema change required: a burst
is a contiguous run of `tool_call` entries between two `message` entries of
the same arc. Bursts close when the next assistant `message` is emitted.

**Phase-2** — explicit `burst_id` (TEXT) on `arc_entries`, set by the agent
loop when it begins a tool-using turn and closed when the assistant emits
its next user-facing reply. Removes ambiguity around interleaved
human-in-the-loop approvals mid-burst.

## 3. Output shape of a compaction

A single compaction pass against an arc produces:

1. **One summary entry** — `EntryType::Summary` (new variant), `source =
   "compactor"`. Free-text prose covering the compacted prefix, structured
   into fixed sections (see §4).
2. **A salience-ranked verbatim tail** — last N hot entries (open actions,
   open approvals, latest outbound per channel) preserved as-is.
3. **A latest-per-tool-series cache** — for each distinct tool name that
   was used in the compacted prefix, the most recent successful result,
   stored as a single `tool_call` entry. This gives the next turn grounded
   "current state" without rehydrating every historical call.

`ArcMeta` gains `summarized_through_entry_id: Option<i64>`. Original entries
are **never deleted** — they remain queryable for the UI ("show full
history") and for future re-compaction with a better algorithm. Only the
*context-build* path uses the compacted view.

## 4. The summarization prompt

Hard-coded categories, not free-form summarization. Mirrors what Claude
Code's leaked compaction prompt does well, adapted to Athen's domain:

- **Arc goal** — what the user/sense set this arc up to accomplish.
- **Participants** — contacts, threads, channels involved.
- **Decisions made** — non-obvious choices the agent or user committed to.
- **Constraints stated** — "don't do X", "always Y", trust signals.
- **Pending approvals** — anything currently waiting on the user.
- **Last tool-call outcomes** — one line per *named* tool series, with
  failure → success pattern preserved if relevant.
- **Open action** — what the agent was about to do next, verbatim.

The prompt explicitly forbids paraphrasing direct user quotes about
constraints or decisions — those carry exact-wording weight.

## 5. Triggers and budgets

Per-model, driven by existing LLM provider config. Add to the model entry:

- `context_window_tokens: u32` — authoritative ceiling for that model
  (200k for Sonnet/Opus on Anthropic, 128k for typical 3rd-party APIs,
  user-configurable for local models that lie about their effective
  window).
- `compaction_trigger_pct: u8` — default `65`. Compact when estimated
  arc-context size exceeds `context_window_tokens * trigger_pct / 100`.
- `compaction_target_pct: u8` — default `30`. Aim for the compacted view
  to fit within this budget (summary + tail + tool-cache).

Trigger at 65% rather than 90%+: the summarizer is itself an LLM call, and
the LLM produces sharper summaries when *its own* context isn't already
saturated. Late compaction produces lossy summaries.

Token estimation v1: `chars / 4` heuristic. v2: per-provider tokenizer if
needed. The trigger does not need exact tokens — it needs a stable
upper-bound estimator.

## 6. Post-compaction verification turn

After a compaction, before the next dispatch, the agent emits a one-paragraph
state restatement: "Here's my current understanding — arc goal is X, open
action is Y, blocked on Z." If this restatement diverges categorically from
the summary (different goal, missing open action), surface a warning rather
than silently recovering. This is cheap insurance against the failure mode
Claude Code users hit: needing to be very explicit on the prompt right after
compaction because the agent silently lost grounding.

## 7. Architecture: a swappable compactor

Compaction is gated behind a trait so the implementation can evolve without
touching call sites:

```rust
// athen-core/src/traits/compaction.rs
#[async_trait]
pub trait ArcCompactor: Send + Sync {
    /// Decide whether `arc_id` needs compaction under `model`'s budget.
    async fn should_compact(&self, arc_id: &str, model: &ModelId) -> Result<bool>;

    /// Run a compaction pass. Idempotent: a no-op if already compact.
    async fn compact(&self, arc_id: &str, model: &ModelId) -> Result<CompactionOutcome>;

    /// Build the LLM context view for `arc_id` under `model`'s budget.
    /// Returns: summary entry (if any) + verbatim tail + tool-series cache.
    async fn load_context_view(&self, arc_id: &str, model: &ModelId) -> Result<ArcContextView>;
}
```

Phase-1 implementation: `LlmArcCompactor` — pure LLM summarization driven
by the prompt in §4, heuristic burst detection from §2, char/4 token
estimation. Lives in `athen-app` (composition root) initially; can be
extracted to its own crate if it grows.

Phase-2+ extension points (kept open, not built now):

- Replace heuristic burst detection with `burst_id` columns + agent-loop
  signaling.
- Add a deterministic *entropy-based* compactor that runs *before* the
  LLM pass: detects exact-duplicate or trivially-similar entries
  (consecutive `read_file` of same path → keep last) and removes them
  losslessly. Cheap, fast, reduces work the LLM has to do.
- Add a *semantic-embedding* salience scorer: embed each entry, cluster
  the prefix, and feed cluster centroids to the LLM as priors for what to
  keep verbatim vs. summarize. Improves recall on long arcs.
- Hierarchical re-compaction: when the post-summary tail itself crosses
  the budget, the existing summary + new tail get re-summarized into a
  single newer summary. LSM-tree-like.

The Phase-1 trait shape supports all of these — they slot in as different
implementations or as preprocessing stages stacked behind the same
interface.

## 8. State model and restart-resilience

Two pieces of durable state, both in SQLite:

1. **Summary rows** — `arc_entries` rows with `entry_type = 'summary'`. Same
   table as every other entry.
2. **A pointer** — `ArcMeta.summarized_through_entry_id: Option<i64>`,
   pointing at the largest `arc_entries.id` covered by the *latest*
   summary.

There is no in-memory "current compacted view." The view is **derived on
every load** by querying SQLite. The four states:

| State | `summarized_through_entry_id` | Tail | Context view |
|---|---|---|---|
| Fresh arc | `None` | n/a | all entries |
| Compacted + new turns | `Some(k)` | `k+1..n` | `S_k` + entries `id > k` |
| Just compacted | `Some(k)` | none | just `S_k` |
| Re-compacted | `Some(k₂)` (`k₂ > k₁`) | `k₂+1..n` | `S_k₂` + entries `id > k₂` |

`load_context_view` is one query: latest summary (by `created_at`,
`entry_type = 'summary'`) + entries with `id > pointer`. Deterministic, no
caching.

**Edges:**

- **No compaction yet** — `summarized_through_entry_id IS NULL`. Loader
  falls through to current `load_entries` behavior. Existing paths
  unchanged for short arcs.
- **All summary, zero new turns** — loader returns `[S_k]` only. The §6
  verification turn runs here and produces the first new entry — the
  agent's restated understanding — which then anchors the new tail.
- **Re-compaction** — older summaries `S_k₁`, `S_k₂`, ... remain in
  `arc_entries` forever. Loader picks only the most recent. Older
  summaries surface as "Earlier summaries" in the UI history. We never
  delete.
- **Mid-compaction crash** — summary insert + `ArcMeta` pointer update in
  one SQLite transaction. Atomic, no half-states.
- **Concurrent writes during compaction** — snapshot entry IDs at the
  start. New entries written while the LLM call is in flight land with
  `id > snapshot_max` and fall outside this summary's coverage. Picked up
  by the next compaction. No locks needed.

**Restart-resilience.** Claude Code has a known issue where compaction is
lost across app restart, leaving the user in a pre-compaction state. The
cause is structural: compaction state living somewhere other than the
canonical conversation log. Athen's design avoids this class of bug by
construction:

- The summary is a first-class row in `arc_entries`, same table and same
  query path as every other entry. No separate "compacted view" object
  can be lost.
- The pointer is a column on `ArcMeta`, persisted on every change.
- Every dispatch re-derives the view from SQLite; there is no in-memory
  "current view" cached across requests.

**The discipline rule.** The executor's context-build path **must never
read raw `arc_entries` directly**. Always go through
`compactor.load_context_view`. The UI can read raw entries (it should —
it's the history view). The executor cannot. This invariant is what
prevents the Claude Code bug from creeping back in via someone adding a
new dispatch path that bypasses the compactor.

## 9. What stays UI-visible

The frontend continues to call `get_arc_entries` (`commands.rs:2685`) and
gets the **full** entry list, including originals that have been compacted
out of the LLM view. Summary entries render as a collapsed
"Earlier in this arc..." block the user can expand. Compaction is purely
an executor-side concern; the UI is the source of truth for "what
happened."

## 10. Cache-friendly context layout

Prompt caching (Anthropic's explicit `cache_control`, DeepSeek's automatic
prefix cache) is **prefix-based**: the cache hits on whatever leading bytes
are byte-identical to a previous request. Any insertion, reorder, or
mutation at position N invalidates everything from N onward. The
compactor is the context-builder, so cache-friendly layout is its
responsibility.

**The law: append-only prefixes, volatile content at the end.**

The Anthropic Messages API and DeepSeek (OpenAI-compatible) both fix the
top-level order: `[tools] → [system] → [messages]`. `tools` and `system`
are top-level parameters, prepended by the provider; we **cannot** put
history before tools. The orderable territory is *within* each section.

By section, from most-stable to most-volatile:

```
[tools]          ← API position 1
  stable tools first (cache_control breakpoint here)
  variable tools after, if any

[system]         ← API position 2
  stable profile prompt (cache_control breakpoint here)
  no volatile content (no dates, no recall, no transient IDs)

[messages]       ← API position 3 — this is where ordering is our call
  arc summary, if any (stable until next compaction)
  arc history (id ASC) — cacheable, append-only
    cache_control breakpoint on last entry
  memory recall injection — volatile, recomputed
  current user/sense turn — volatile by definition
```

**Reference files vs working files.** The static header (system prompt
+ tools) is also the home for ambient reference content the agent
always wants loaded — profile-level instructions, project context like
a CLAUDE.md analogue. These change rarely (user-initiated, out-of-band)
and benefit from the stable-prefix slot. *Working files* — files the
agent reads and edits as part of a task — are tool-call results that
already flow through `arc_entries`; they stay in history and are
collapsed by the compactor's per-tool-series cache (re-reads of the
same path collapse to "latest content at hash H"; edit/read bursts
collapse to "edited foo.rs N times, final state is..."). Do not try
to lift working files into a stable slot — their volatility is
intrinsic and any stable position would be a lie within a few turns.

**Reference docs are out of scope for the compactor.** Athen will grow
CLAUDE.md-equivalent reference docs (global, per-profile, per-arc) the
user maintains by hand. The compactor must NOT read, summarize, or
budget these:

- They live in their own store (separate table or columns), not in
  `arc_entries`.
- They are injected at executor-build time **AFTER** the compactor
  produces its view, prepended/appended to the system prompt as part
  of the static header — not into the messages array.
- The user is responsible for keeping them concise. The compactor's
  budget calculations only count `arc_entries` content; reference
  docs are budget-free from the compactor's perspective (their cost
  shows up in the static header, which the cache covers).
- Updating a reference doc invalidates the static-prefix cache once,
  then it stabilizes again.

This boundary is the discipline rule from §8 applied to a different
axis: anything the user maintains directly is the user's
responsibility; only `arc_entries` (which the agent and senses
generate) is the compactor's territory.

**Content-addressed read results.** A small tool-layer optimization:
when `read` runs, record the file's content hash; subsequent reads of
the same path at the same hash return byte-identical results. This is
tool-result memoization, not prompt caching, but it preserves prompt
determinism — a "re-read" of an unchanged file produces the same
prompt bytes as the previous read and so doesn't break the prefix when
that entry persists in history.

**Tools stability is per-profile, not per-task.** Tool variance within
an arc kills the tools-prefix cache, which then cascades — *all*
downstream caching dies because the tools section is the leading prefix.
If per-task tool tiering is needed, do it as Anthropic recommends: stable
tools first, `cache_control` breakpoint after them, variable tools at
the end of the tool list. This aligns with the existing rule that
ToolSelection tiers tools by prominence rather than blocking — cache
friendliness is a second reason for that choice.

Anthropic's `cache_control` allows up to 4 breakpoints. Place them at:

- End of system prompt + tools
- End of summary (if present)
- End of arc history

The volatile suffix (memory recall + current turn) is recomputed every
turn — cheap, because it's small.

**Known violation in current code (must fix before Phase 1 lands):**
`commands.rs::execute_approved_task` and `execute_dispatched_task`
currently prepend memory-recall results to the context with
`context.insert(0, ChatMessage { role: System, content: "MEMORIES ALREADY
LOADED..." })`. That puts a volatile block at position 0, ahead of all
arc history, and invalidates the entire arc-history cache every turn
where recall returns different items. The fix: append memory injections
*after* arc history and *before* the current turn.

**Hidden cache-busters to audit:**

- **HashMap iteration order** — not deterministic. Wherever tool args,
  profile config, or context are serialized from a `HashMap`, switch to
  `BTreeMap` or sort keys before serializing.
- **Timestamps in static prompt content** — never inject "current time"
  into the system prompt or any static prefix. If the agent needs the
  current time, it asks via a tool. Timestamps inside *persisted entry
  content* are fine (each entry's `created_at` is stable forever).
- **Random IDs in prompt-visible text** — arc IDs, task IDs surfacing in
  rendered prompt text are stable per-arc, so safe; transient request
  IDs are not, so strip them.
- **Tool result formatting** — must not include elapsed-time or
  per-request randomness in the model-visible result. The auditor's
  truncated `detail` must be deterministic given identical inputs.
- **Rehydration ordering** — arc entry queries must sort by `id ASC` (or
  `created_at, id`); sorting by `created_at` alone allows ties to flip
  order across loads.
- **JSON serialization stability** — `serde_json` is field-order stable
  per struct definition. Round-tripping through `serde_json::Value`
  re-orders to alphabetical, which breaks the prefix. Avoid in
  prompt-construction paths.

**Compaction itself breaks the cache once.** Writing a new summary
shortens the prefix and changes its tail bytes; cache misses for one
turn. That cost is the price of the compaction; the *next* turn lands a
new cacheable prefix that grows append-only until the following
compaction. So cache-effectiveness is bounded below by the
compaction-frequency: triggering at 65% rather than 90% means more
compactions, but each compaction follows a longer cached run, so the
amortized cost still favors earlier compaction.

## 11. Phased rollout

**Phase 1 — minimum viable compaction** (the starting point):

1. Add `EntryType::Summary` variant + `summarized_through_entry_id` column
   on `ArcMeta`. Schema migration.
2. Add `context_window_tokens` + `compaction_trigger_pct` +
   `compaction_target_pct` to the LLM provider config.
3. Implement `LlmArcCompactor` with the §4 prompt, §2 heuristic burst
   detection, char/4 token estimation.
4. Replace direct `load_entries` calls in `execute_approved_task` and
   `execute_dispatched_task` with `compactor.load_context_view`.
5. Add the post-compaction verification turn from §6 (logged on divergence,
   not blocking initially).

**Phase 2 — observability and tuning:**

- Log compaction events (`arc_id`, before/after token estimate, summary
  length, latency).
- Manual compaction: the `compact_arc(arc_id)` Tauri command lands in
  Phase 1 (forced compaction via `target_tokens = 0` on the trait
  `compact` method); a `/compact <arc_id>` REPL variant for the CLI is
  Phase-2.

**Phase 3 — quality improvements:**

- Explicit `burst_id` from the agent loop.
- Deterministic pre-pass (entropy/dedup).
- Embedding-driven salience scoring.
- Hierarchical re-compaction.

Phase 1 is the smallest end-to-end slice that delivers value (arcs no
longer break when long). Phases 2–3 are improvements layered on the same
trait surface.

## 12. Implementation status (2026-05-19)

This section is the live status map; sections 1–11 above are the design.
Update this section, not the design body, when implementation moves.

### What's live

| Design section | Status | File:line |
|---|---|---|
| §3 Summary entry + `summarized_through_entry_id` pointer | ✅ | `EntryType::Summary` row written via `ArcStore::compact_arc`; `ArcMeta.summarized_through_entry_id` updated in the same transaction |
| §3 Latest-per-tool-series cache | ✅ | `crates/athen-app/src/compaction.rs:464–483` — `BTreeMap` keyed by tool name, scans `id ≤ cutoff_id` of type `ToolCall`, keeps latest per tool |
| §4 Hard-coded summarization prompt | ✅ | `crates/athen-app/src/compaction.rs:297–313`; `ModelProfile::Fast`, `max_tokens=2048`, `temperature=0.0` |
| §5 Per-model budgets + trigger thresholds | ✅ | `ProviderConfig.context_window_tokens` / `compaction_trigger_pct` / `compaction_target_pct` (defaults 128k / 65 / 30); resolution in `resolve_compaction_budget` |
| §2 Phase-1 burst heuristic (keep last 25% verbatim) | ✅ | `compaction.rs:347–358`; refuses to compact if <4 entries |
| §7 Trait shape | ✅ | `athen-core::traits::compaction::ArcCompactor` |
| §7 `LlmArcCompactor` Phase-1 implementation | ✅ | `crates/athen-app/src/compaction.rs:318–484` (the trait impl + helpers) |
| §8 Atomic SQLite write (summary + pointer) | ✅ | Inside `arc_store.compact_arc()` |
| §8 Discipline rule: executor goes through `load_context_view` | ✅ | Executor path runs `compactor.prepare_context(...)` at `crates/athen-app/src/commands.rs:2939, 3973`; `view_to_messages()` at `compaction.rs:108–145` converts to `(Vec<ChatMessage>, system_suffix)` |
| §9 UI shows full history including originals | ✅ | Summary entries render as collapsed `<details>` block "Earlier in this arc" in `frontend/app.js:2467` |
| §10 Memory recall fix (no `context.insert(0, ...)`) | ✅ | Memory now flows through `external_system_suffix` appended to leading system prompt; the §10 violation is resolved |
| §11 Phase-1.5 manual `compact_arc` Tauri command | ✅ | `crates/athen-app/src/commands.rs:3653`; force via `target_tokens = 0` |
| Settings UI for budgets | ✅ | `frontend/app.js:6222–6232` (`provider-context-window`, `provider-compaction-trigger`, `provider-compaction-target`); validators in `crates/athen-app/src/settings.rs:791–839` |

### What's not yet implemented

| Design section | Status | Notes |
|---|---|---|
| §6 Post-compaction verification turn | ❌ | The "state restatement before next dispatch" + divergence warning hasn't landed. Highest-leverage gap left in Phase-1 — without it, divergence is silent. |
| §2 Phase-2 explicit `burst_id` column | ❌ | Heuristic 25% rule is good enough for now; revisit if interleaved approvals start producing wrong burst boundaries. |
| §7 Phase-2 deterministic entropy/dedup pre-pass | ❌ | Trait shape supports it as a stage; not implemented. |
| §7 Phase-3 embedding-driven salience | ❌ | Trait shape supports it; not implemented. |
| §7 Phase-3 hierarchical re-compaction | ❌ | The data model handles re-compaction (older summaries stay in `arc_entries`); the *pass* that combines `S_old + tail → S_new` when the tail itself overflows is not yet wired. |
| §10 HashMap iteration audit, JSON-roundtrip audit | ⚠️ | The known cache-buster (`context.insert(0, ...)`) is fixed; a broader audit of remaining HashMap → BTreeMap conversions in prompt-construction paths is still open. |

### Test coverage

`crates/athen-app/src/compaction.rs::tests` — 9 tests:

- `estimate_tokens_chars_div_four` — token estimator
- `resolve_compaction_budget_uses_active_provider` — budget resolution
- `resolve_compaction_budget_falls_back_for_unknown_provider` — fallback defaults
- `resolve_compaction_budget_clamps_trigger_above_target` — hysteresis clamp
- `resolve_provider_temperature_reads_active_override_or_returns_none` — temperature resolver
- `build_summary_prompt_tags_roles_and_strips_newlines` — prompt construction
- `load_context_view_with_no_summary_returns_all_tail` — loader fresh arc
- `view_to_messages_preserves_tool_calls_across_rehydration` — tool-call rehydration across provider failover
- `load_context_view_with_summary_returns_summary_plus_tail` — loader post-compaction

Plus `crates/athen-core/src/traits/compaction.rs:125–164` — dyn-compatibility sanity check on the trait.

### Known limitations to keep in mind when building on this

- Token estimation is `chars / 4`. Stable upper-bound, but not exact —
  per-provider tokenizers haven't been wired.
- Compaction is **always-on** when a budget is configured; there is no
  per-arc opt-out toggle. Add only if real users ask.
- The summary prompt is hard-coded English. Localization policy for it is
  open (see `project_athen_small_model_gaps.md`).
- The `load_context_view` query is one round-trip per dispatch. Not cached;
  cheap so far, audit if dispatch latency regresses.
