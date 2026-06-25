# Deep Research (Design Doc)

**Status (2026-06-25):** SHIPPED (button/command trigger). Implemented on branch
`feat/deep-research`. Workspace builds clean (clippy + tests green). Code is
authoritative; sections below remain the conceptual reference.

Implementation map: `crates/athen-app/src/deep_research.rs` (the pure orchestrator
`run_deep_research` — plan/fan-out/synthesize, decoupled from `AppState` via a
worker-spawn closure + progress callback); `AppState::run_deep_research_for_arc`
(state.rs — builds the bare worker registry + delegation context, wires progress to
`deep-research-progress` UiBridge events); `deep_research_core` + the `deep_research`
Tauri command + `POST /api/arcs/{id}/deep-research` (commands.rs/http_api.rs — extend
-vs-new, save via `save_file`, stamp arc metadata, emit `deep-research-done`);
`get_research_paper` + `GET /api/arcs/{id}/research-paper` (read-by-arc-id, no
path-traversal surface); `deep_research_worker` built-in profile (profiles.rs);
`arcs.research_paper_path`/`research_question` (arcs.rs); desktop UI (frontend/) +
web UI (web/src, `DeepResearch.tsx`) — trigger dialog, progress banner, result card,
in-app "View paper" markdown modal.

**Two divergences from the design below, decided during the build:**
1. **Decentralized discovery (not central discover-then-partition).** §3 describes a
   discovery phase that collects+dedups URLs and partitions ~5 per worker. The build
   instead decomposes into N sub-questions (N per depth) and gives **each worker its
   own sub-question to search + read ~5 sources for** — a cleaner reuse of
   `run_delegation` (one worker = one delegated sub-agent) that sidesteps the
   "assign 5 sources before you have the list" trap by letting each worker find its
   own. Trade-off: possible source overlap across workers, deduped at synthesis. The
   user's "~5 sources per agent" intent is preserved.
2. **Agent-callable `deep_research` tool deferred.** §6 lists both a UI affordance and
   an LLM-callable tool. Only the UI/command/HTTP trigger shipped. The tool needs a
   runner closure injected into the per-arc tool registry (it can't hold `&AppState`),
   which is a `state.rs`/`app_tools.rs` refactor; deferred as a fast-follow. Today the
   agent can still answer questions about a finished paper (it reads the saved file),
   it just can't self-*initiate* a run — the user triggers it from the UI.

This doc was originally grounded in a code audit of the existing delegation/fan-out
machinery (citations inline); where the audit corrected an assumption, the correction
is called out.

Deep Research is a triggered, long-running workflow that turns a question into a
**cited markdown paper**: the agent decomposes the question, discovers a large
set of sources, fans out **N concurrent workers** (each reading ~5 sources),
joins their structured findings, and synthesizes a paper saved into the
workspace. The user reads it, or asks the main agent follow-up questions about it
in the same arc. Re-triggering in the same arc asks whether to **extend the
existing paper or start a new one**. This is Athen's native answer to
Gemini/Perplexity/OpenAI "deep research" — but artifact-first (a real file in the
Projects/Outputs layout) and proactivity-ready (a wake-up can fire it).

## Motivating moment

The user asks a question that no single search answers — "what's the state of
EU right-to-repair law and how does it compare to California's?" One agent doing
serial searches is slow, shallow, and forgets sources as its context fills.
Deep Research instead spreads the reading across many workers in parallel,
preserves every source's findings as a structured fold, and leaves behind a
durable, cited document the user (and the agent) can return to.

---

## 1. What already exists (audited 2026-06-25)

The substrate is mostly here. The audit's findings, with citations:

### 1a. Delegation — `spawn_subagent`
- Tool name + legacy alias: `crates/athen-core/src/subagent.rs:17` (`spawn_subagent`,
  alias `delegate_to_agent`).
- `ToolDefinition` + handler: `crates/athen-app/src/delegation.rs:167` (schema:
  `target_profile_id`, `brief`, optional `reasoning_effort`).
- Core run: `run_delegation()` `crates/athen-app/src/delegation.rs:432`. Creates a
  **sub-arc** (`create_arc_with_parent`, `arc_<ts>_sub_<uuid>`), synthesizes a Task
  from the brief, builds a fresh sub-executor via factory closures, runs it,
  post-verifies, returns a `ToolResult` with `{sub_arc_id, content, success,
  verified, verification_note}`.
- Pin inheritance: `propagate_parent_pins()` `delegation.rs:318` copies the parent
  arc's `pinned_provider_id` + `pinned_slug`, `tier_override`, and
  `reasoning_effort_override` to the sub-arc; the per-call `reasoning_effort` param
  is applied after and wins.
- Deliverable verification: `verify_deliverable` `delegation.rs:364` — reusable
  per-worker quality gate.

### 1b. Concurrency — the fan-out seam **already works**, with one caveat
- Batch tool execution is genuinely concurrent: the executor builds one async
  block per tool call in the LLM's response and awaits them with
  `futures::future::join_all(dispatches)` — `crates/athen-agent/src/executor.rs:2473`.
  So if the model emits **N `spawn_subagent` calls in one turn, they run in
  parallel** and results thread back in order. Individual failures are isolated
  (each becomes a `Failed` step; siblings still return).
- **Caveat — in-batch dedup guard:** identical tool calls within one batch are
  short-circuited to a stub ("Duplicate call in batch…") `executor.rs:~2515`.
  Fan-out briefs must therefore be **distinct** (they will be — each worker owns a
  different source slice), or the guard silently drops the duplicates.
- **No concurrency cap exists today** — there is no semaphore on tool dispatch or
  on `run_delegation`. Spawning 8 workers that each fan out to web fetches means a
  large burst of in-flight tokio tasks + LLM calls with nothing throttling them.
  Deep Research **must add its own cap** (see §4).

### 1c. Worker tools — `web_search` + `web_fetch`
- `web_search`: `crates/athen-agent/src/tools.rs:3701` (DuckDuckGo default via the
  pluggable `WebSearchProvider`; results clamped to 20 upstream).
- `web_fetch`: `do_web_fetch` `crates/athen-agent/src/tools.rs:2433` — turns a URL
  into clean markdown via the `PageReader` (`HybridReader` default,
  `tools.rs:792`), returning `{url, title, content, source, content_chars}`.
  *(The audit's first pass wrongly reported "no page reader"; `web_fetch` is the
  reader, backed by `athen-web`'s `PageReader`/`HybridReader` — Jina/Wayback/
  Cloudflare/Local providers.)*
- Each research worker's loop is just: `web_search(sub-question)` → pick links →
  `web_fetch(url)` ×~5 → return structured findings.

### 1d. Persistence + container — already shipped
- `save_file(category, project, filename, content)` `crates/athen-app/src/app_tools.rs`
  resolves a workspace bucket and delegates to `write` (checkpoint/snapshot
  inherited). The paper lands in `Outputs/` (loose) or `Projects/<slug>/` (when the
  arc belongs to a project).
- Projects give the paper a home and inject it into the arc via the project file
  listing + summary context layers (see `docs/PROJECTS.md`).

### 1e. What does NOT fit
- **Coordinator multi-agent dispatch** (`crates/athen-coordinador/src/dispatcher.rs:43`,
  `execute_dispatched_task` `crates/athen-app/src/state.rs:3463`) *is* parallel, but
  it is external-event-driven (email/telegram/wake-up → queue → approval →
  dispatch) and **not reachable from inside an agent's tool call**. Deep Research
  fan-out should NOT ride the coordinator. It rides the delegation/`join_all` seam.

**Verdict:** parallel fan-out is real and reachable. We do **not** need to build a
concurrency engine. We need a **first-class orchestrator** that drives the
fixed Deep Research pipeline deterministically (rather than hoping the LLM
batches its calls correctly), adds a concurrency cap + partial-result tolerance,
and owns the paper lifecycle.

---

## 2. Why a first-class orchestrator, not LLM-improvised batching

The cheap path is "tell the main agent to emit N `spawn_subagent` calls and
synthesize." Rejected as the primary mechanism because:

1. **Determinism.** The workflow is fixed (plan → discover → fan-out →
   synthesize). Relying on the model to emit exactly-N unique parallel calls,
   then re-read N results and synthesize, is fragile and varies per provider.
2. **Partial results.** If a worker fails, an orchestrator can proceed with the
   survivors and *record* the gap in the paper. In raw batching, a failed
   `spawn_subagent` is just a `Failed` step the model may mishandle.
3. **Cost control.** Depth/budget caps (§4) need a real loop with a semaphore;
   the model can't be trusted to self-limit fan-out.
4. **Lifecycle.** Paper persistence, the modify-vs-new state machine (§5), and
   follow-up Q&A are orchestration concerns, not prompt suggestions.

So: build `deep_research` as an **app-level orchestrator** that *internally*
reuses `run_delegation` futures under `join_all` + a semaphore. Same proven
seam (§1b), wrapped in a deterministic harness.

---

## 3. The pipeline

Four phases. Phases 1, 2, 4 are single LLM passes; phase 3 is the fan-out.

1. **Plan / decompose** — one Fast-tier LLM pass turns the question into 4–8
   sub-questions + search angles (multi-modal sweep: by-entity, by-time,
   by-claim). Output is structured (a list), not prose.
2. **Discover** — run `web_search` for each angle, collect candidate URLs,
   **dedup by normalized URL**, rank, and **partition** into worker slices of
   ~5 sources each. *Discovery precedes partitioning* — you cannot assign "5
   sources per worker" until the source set exists. This is the step naïve
   designs skip.
3. **Read (fan-out)** — spawn one worker per slice via the orchestrator. Each
   worker is a sub-agent (a `deep_research_worker` profile, §6) whose brief is
   "read these ~5 URLs, answer this sub-question, return findings + citations."
   Workers return **structured findings** (claim, evidence, source URL,
   confidence) — not prose — so synthesis has clean inputs. Run under
   `join_all` + a semaphore (§4). Failed/empty workers are tolerated.
4. **Synthesize** — one higher-tier LLM pass folds all findings → a cited
   markdown paper (sections + inline `[n]` citations + a sources table +
   an explicit "gaps / what couldn't be verified" section listing failed
   slices). `save_file` it; record its path + the question on the arc (§5).

Each worker is a real sub-arc (parent-linked), so the existing arc tree + the
pending delegation-expansion UI surface them for audit.

---

## 4. Cost & concurrency control (depth budget)

Deep Research is expensive (dozens of fetches + many LLM calls). It ships with a
**depth knob** mirroring Gemini's, bounding the blast radius:

| Depth | Sub-questions | Workers | Sources/worker | Synthesis tier |
|-------|---------------|---------|----------------|----------------|
| `quick` | 3 | 3 | 4 | Fast |
| `standard` (default) | 5 | 5 | 5 | Standard |
| `deep` | 8 | 8 | 6 | Heavy |

- **Semaphore.** The orchestrator caps concurrent workers (e.g. `min(depth.workers,
  4)` permits) so a `deep` run doesn't open 8×6 simultaneous fetches. This is the
  cap §1b says doesn't exist yet — it lives in the orchestrator, not the executor.
- **No silent truncation.** If discovery finds more sources than the budget reads,
  `log` it and note it in the paper's gaps section — never quietly drop coverage.
- **Provider tiering.** Workers run on the cheap/Fast tier (reading + extraction);
  synthesis runs on a stronger tier. Both inherit the arc's pin via
  `propagate_parent_pins` (§1a) unless overridden.

---

## 5. Re-trigger: modify vs. new (per the user's spec)

State lives on the arc (new arc-metadata fields, not a new table for v1):
`research_paper_path: Option<String>` + `research_question: Option<String>`.

- **First trigger in an arc:** run the full pipeline, persist path + question.
- **Re-trigger in the same arc:** the orchestrator sees an existing
  `research_paper_path` → asks the user (via the existing question/approval card):
  **"Extend the existing paper on _X_, or start a new research paper?"**
  - **Extend** → incremental fold: discovery + fan-out run on the *new* angle, and
    synthesis takes `prior paper + new findings → revised paper` (same shape as the
    project compactor's prior-summary-plus-delta fold, `compaction.rs`). Overwrites
    the same file (checkpointed via `write`, so revertible).
  - **New** → fresh paper, new filename; the old one stays on disk.

Follow-up Q&A needs no new machinery: the paper is a file in the workspace, and
the Projects file-listing/summary context layers already make the agent aware of
it. "Ask the main agent about it" = the agent `web_fetch`/`read`s its own paper.

---

## 6. Implementation sketch (where the code goes)

- **Worker profile.** New `deep_research_worker` agent profile (read-only tool set:
  `web_search`, `web_fetch`, scratch `read`), so workers can't write/email/etc.
- **Orchestrator.** New module `crates/athen-app/src/deep_research.rs`:
  - `plan(question, depth) -> Vec<SubQuestion>` (LLM, Fast).
  - `discover(sub_qs) -> Vec<SourceSlice>` (`web_search` + dedup + partition).
  - `fan_out(slices) -> Vec<WorkerResult>` — `join_all` over per-slice
    `run_delegation`-style futures, gated by a `tokio::sync::Semaphore`; tolerant
    of `Err`/empty (collect `Result`s, keep survivors).
  - `synthesize(question, results, prior_paper?) -> String` (LLM, higher tier).
  - `save + stamp arc metadata`.
- **Tool surface.** A `deep_research(question, depth?)` agent tool (so the main
  agent can trigger it mid-conversation) **and** a UI affordance (a "Deep Research"
  button on the arc composer). Both call a shared `deep_research_core(&AppState,
  arc_id, question, depth)`.
- **Command + HTTP route** sharing that `_core` fn (`commands.rs` Tauri command +
  `http_api.rs` route in `full_surface_router()`), per the project's parity rule.
- **UI (desktop `/frontend` + web `/web`)** — a progress surface: phase ticker
  (planning → discovering N sources → reading N/M → synthesizing), worker sub-arc
  expansion, and a "view paper" link when done. Both UIs at parity.
- **Supervision.** Reuse the hardening patterns already in the tree:
  `AssertUnwindSafe(...).catch_unwind()` around each worker future, errors
  surfaced not dropped, partial results persisted before synthesis so a synthesis
  crash never throws away completed reads.

---

## 7. Risks

- **No existing concurrency cap (§1b).** Must add the semaphore in the
  orchestrator; without it `deep` depth can stampede the provider + network.
- **In-batch dedup guard (§1b).** If the orchestrator ever degrades to LLM
  batching, identical briefs get stubbed — keep slices distinct.
- **Source quality / hallucinated citations.** Synthesis must cite only URLs that
  appear in worker findings; a cheap post-check can verify every `[n]` maps to a
  fetched source. Diverse-lens verification is a later upgrade.
- **Cost.** The depth budget + Fast-tier workers are the guardrail; surface an
  estimate before a `deep` run.
- **Paper overwrite on Extend.** Goes through `write` → checkpointed/revertible;
  keep that path (don't bypass the snapshot layer).
- **Latency.** A `deep` run is minutes, not seconds — it must be a backgrounded,
  progress-reporting task, never a blocking tool call that times out the turn.

---

## 8. Sequencing

1. `deep_research_worker` profile + the structured-findings brief/return shape.
2. Orchestrator `plan`/`discover`/`fan_out` (semaphore + partial tolerance) +
   `synthesize`, wired to `save_file`. Reuse `run_delegation` internals.
3. `deep_research` tool + `_core` + Tauri command + HTTP route.
4. Modify-vs-new arc metadata + the re-trigger question + Extend fold.
5. Desktop + web progress UI (parity).
6. Later: wake-up-triggered scheduled research; diverse-lens citation
   verification; depth auto-selection from question complexity.

## 9. Verification

- Unit: discovery dedup + partition (URL normalization, slice sizing per depth);
  semaphore caps in-flight workers; partial-result tolerance (1 worker errors →
  paper still written with a gaps note); modify-vs-new branch picks Extend vs New
  correctly from arc metadata.
- Integration (mocked search/reader + mocked router): full pipeline produces a
  cited paper; every `[n]` resolves to a fetched source; re-trigger → Extend
  overwrites + folds, New writes a second file.
- Manual (`cargo tauri dev`): run `standard` depth on a real question, watch the
  phase ticker + worker sub-arcs, open the paper, ask a follow-up, re-trigger →
  pick Extend → confirm the paper grows. Repeat in the web UI for parity.
