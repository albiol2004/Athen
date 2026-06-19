# Projects (Design Doc)

**Status (2026-06-20):** SHIPPED — §7 steps 1–3 (workspace folder layout,
Project entity, project-wide compaction). Only step 4 (sense auto-routing)
remains deferred. Two decisions diverged from the original design during
implementation: project instructions live in a dedicated `instructions` column
on the projects row (not the `ProfileTag::Project` Identity reuse §1 sketched —
that axis is orthogonal to projects and cost ~8–10 files for no gain), and
renaming a project **renames its workspace folder** to match (folder_slug tracks
the name). End-to-end verified via the headless HTTP API (CRUD, folder
create/rename-moves-folder, summary-mode persistence across restart, arc
assignment). Code is authoritative; the sections below remain the conceptual
reference.

Implementation map: `ProjectStore` (`crates/athen-persistence/src/projects.rs`:
projects + project_arc_folds + project_meta tables, CRUD, slug, summary +
watermark + meta helpers); `arcs.project_id` column + `create_arc_in_project` /
`set_arc_project` (`arcs.rs`); `seed_workspace_skeleton` (`athen-core/paths.rs`);
`save_file` category tool (`app_tools.rs`); `LlmProjectCompactor.fold_arc_into_
project` (`compaction.rs`) fired best-effort at arc transitions; context layers
1–4 + project CRUD `*_core` commands + `maybe_fold_leaving_arc` (`commands.rs`);
HTTP routes (`http_api.rs`); desktop panel (`frontend/`) + web `PanelProjects.tsx`.

A **Project** is a named, durable container that groups *many arcs* around a
common piece of work and shares context across them — the feature users love
in ChatGPT/Claude, made Athen-native by senses and proactivity. It is the
organizing principle the workspace-folder idea was reaching for: instead of a
new silo, a Project is a single **id** that arcs, Identity, Memory, and a
workspace folder all hang off.

## Motivating moment

The user wants to ask Athen about their phone contract and realizes there is
nowhere canonical for "things about me." Pull that thread and two needs fall
out:

1. **Knowledge about the user** ("which contract do I have") — already served
   by Identity / Memory; a *file* is the wrong home for a recalled fact.
2. **Filing real files** — downloads, generated artifacts, multi-file work —
   which is unmet, and which a structured workspace solves.

Projects unify both: a project folder holds the *artifacts* (the contract PDF,
insurance docs), and a Memory/Identity entry holds the *pointer* ("phone
contract in `Projects/<name>/` or `UserInfo/`"). Files store documents; the
knowledge store stays the thing actually in the prompt.

## 1. A Project is a context-scope above arcs

A Project is not a big new subsystem. It is an id that existing subsystems key
on:

| Facet | Backing store | Notes |
|-------|---------------|-------|
| Folder | Workspace `Projects/<name>/` | Artifacts; see §5. |
| Member arcs | `arcs.project_id` (new optional column) | Many conversations, not one. |
| Project instructions | Identity entry, `applies_to = project:<id>` | Exactly ChatGPT "project custom instructions" — no new storage model. |
| Project memory | Memory rows tagged with project scope | Memories from any arc, preferentially recalled in siblings. |
| Project summary | New `project_summaries` row (see §3) | The maintained cross-arc context. |

The substrate already exists (arcs, Identity `applies_to`, hybrid Memory,
`ArcCompactor`). The Project is the keyring that ties them together.

## 2. Context sharing — layered, cheapest first

The naive design concatenates every arc in the project into each new arc and
blows the context window (a real concern for small/local models — see
`docs/PROMPT_SIZE_MODES` intent and the prompt-cache discipline). The right
design layers signals, heaviest last, and each layer is independently
skippable:

1. **Project instructions** — tiny, always in the prompt (scoped Identity).
2. **Project file listing** — names in the prompt, content read on demand. Cheap.
3. **Project-scoped recall** — boost memories created in sibling arcs.
4. **Project summary** — the one LLM-cost layer; see §3. Optional and
   degradable.

Cache-safety: like memory injection, project context is volatile and belongs
toward the **end** of the prompt body, never prepended to the static prefix
(cf. `feedback_prompt_cache_optimization`).

## 3. Project-wide compaction (incremental, hierarchical)

The project summary is **hierarchical compaction one level up** — exactly the
deferred Phase-2/3 item in `docs/ARC_COMPACTION.md` (§ hierarchical
re-compaction), reused rather than reinvented.

**Trigger.** Fold the *just-left* arc's delta into the project summary when an
arc goes inactive (new-arc creation or arc-switch within the project). This is
O(1) per switch — incremental — never an O(n) re-summarize-everything sweep.

**What gets folded.** The arc's *already-existing* compaction summary, not its
raw transcript. The arc is compacted anyway for context-window reasons, so the
marginal project cost is:

```
project_summary' = compact(project_summary + delta_from_last_arc)
```

on the cheap Bundle tier. Small, because it compacts summaries, not
transcripts.

**Two refinements that make it correct and cheap:**

- **Per-arc watermark** — store the last arc entry already folded into the
  project summary. On the next fold, include only entries past the watermark.
  This lets the user return to an arc later without double-counting its earlier
  content.
- **Dirty-flag + min-delta gate** — mark an arc dirty on meaningful activity;
  only fold on deactivation when it accrued a minimum delta. Trivial flips
  between arcs then cost zero tokens.

## 4. Cost control (local models / token-averse users)

The summary layer must be **degradable**, because some users run local models
or simply don't want the spend:

- **Off** → project context falls back to the non-LLM layers: instructions +
  file listing + recent arc titles. Zero tokens, still useful.
- **Cheap tier** → compaction runs on the Fast/cheap Bundle tier (as arc
  compaction already does), not the user's main model.
- **Manual trigger** → an "Update project summary now" action for users who
  want to control *when* they spend, optionally alongside the automatic
  arc-switch trigger.

Default leans conservative: the cheap layers are always on; the summary layer's
default (on-cheap vs off) is a settings decision, not load-bearing for the
design.

## 5. Workspace folder layout — ship this part early, standalone

Projects link cleanly to an opinionated workspace layout. The folder structure
is worth shipping *before* the full Project entity, as long as it is built
**forward-compatible**.

- A handful of obvious, shallow top-level buckets inside `ATHEN_WORKSPACE_DIR`
  (the root already exists from the TerminalBench work): e.g. `UserInfo/`,
  `Downloads/`, `Projects/<name>/`, `Notes/`, `Outputs/`. Resist a deep
  taxonomy — LLMs file cleanly into 4–6 clear buckets and mis-file across
  fifteen.
- Full filesystem access is retained; this layout is **purely additive** for
  work inside the workspace.

**The LLM will not respect a documented convention alone** — reliability comes
from mechanism, not hope:

1. **Folder map in the prompt at decision time** — a tiny tree in the static
   prefix or a `<system-reminder>` (already injected every 3rd iteration). A
   layout only documented in a file the agent never reads is ignored.
2. **Tools do the filing, not the model** — give save/write tools a *semantic
   category* (`save_file(category: "user_info" | "download" | "project",
   project: "...")`) and resolve the real path in code. Picking a bucket from a
   short list is reliable; composing consistent paths drifts. This moves
   correctness from LLM discipline to enforced-by-code.
3. **Seed the skeleton at boot** — create the dirs with a per-folder `README`
   describing what belongs there; the READMEs double as in-context hints when
   the agent lists the workspace.

**Forward-compatibility hook:** add the optional `project_id` column to arcs
and use the `Projects/<name>/` bucket from day one. The folder feature then
ships standalone, and when the full Project entity lands there is no migration
and no corner-painting.

## 6. The Athen-native differentiator: sense auto-routing (future enhancement)

ChatGPT/Claude Projects are **passive** — the user manually starts a chat
inside one. Athen has senses, so a Project can **auto-route incoming reality
into itself**: an email about the kitchen renovation lands → Athen recognizes
it belongs to the "Kitchen Reno" project → files the attachment in the folder,
continues that project's context, nudges with the project's accumulated state.

This is the same machinery as `docs/MULTI_INTENT_ROUTING.md` (#152) one level
up: route to the right *project*, then the right *arc*. Nobody else can do this
because nobody else has the senses feeding it — it is what makes Athen's
Projects better than parity, not equal to it.

**Explicitly future work.** It depends on multi-intent routing landing first
and is not part of the initial Project entity. Listed here so the routing
design accounts for a project tier when it is built.

## 7. Suggested sequencing

1. **Now, standalone:** workspace folder layout + category-driven save/write
   tools + boot skeleton + optional `arcs.project_id`. Forward-compatible,
   immediately useful, fixes the phone-contract moment via `UserInfo/` +
   Memory pointer.
2. **Project entity:** Project store (id/name/folder/instructions/member arcs),
   Settings UI, scoped Identity + Memory, the four-layer context sharing of §2.
3. **Project-wide compaction:** incremental hierarchical fold of §3 with
   watermark + dirty-gate, on the cheap tier, degradable per §4.
4. **Sense auto-routing (future):** project tier on multi-intent routing (#152).
