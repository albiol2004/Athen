# Code Mode

> **Status:** BUILDS 1 + 2 SHIPPED (2026-06-25/26, `feat/code-mode`). Code is
> authoritative. Live: per-arc flag + root, per-arc cwd/fs-base + sandbox
> widening, git recognition layer + command/route, `[CODE MODE]` prompt block,
> desktop+web toggle + Code-Mode panel. §6 undo resolved to **option (b)** — the
> shadow checkpoint store stays active for per-action undo, plus a GitLens-style
> discard. The only deferred items are the §10 "Build 3" list (live sub-agent
> transcript streaming, generalized progress events, worktree auto-suggest).

## 1. What Code Mode is (and is not)

**Code Mode** is a **per-arc, UI-only posture** for power users doing software work.
When an arc is in Code Mode, Athen stops treating its filesystem activity as
generic "save files into the workspace" and instead treats the arc as a **coding
session rooted in a real git repository**: the agent works *in-place* in that
repo, real `git` is recognized and visualized, and the agent may spin up **git
worktrees on demand** when it fans out parallel sub-agents.

Code Mode is **orthogonal to agent profiles**. A profile answers *who the agent
is* (which tools it leans on, its persona) — and the existing profiles already
code well; we keep them as-is. Code Mode answers *what environment the arc runs
in* (a real repo, real git, worktree isolation, a richer activity/diff surface).
The two compose: any profile can run in a Code-Mode arc.

What Code Mode is **not**:

- **Not app-wide.** Senses (email/calendar/messaging/telegram) spawn arcs
  autonomously; a global "everything is code mode" posture would wrongly drag
  worktree/branch machinery into an email-triage arc. Code Mode is a flag the
  **desktop / web client sets on a specific arc**. `sense_router.rs` never sets
  it, so sense-created arcs are always `code_mode = false`.
- **Not a new concurrency engine.** Parallel sub-agents reuse the existing
  delegation fan-out (`run_delegation`, the executor's `join_all` seam) exactly
  as Deep Research does. Worktrees only make that fan-out *safe to run against a
  shared repo*.
- **Not a git abstraction layer.** The agent already drives git through the shell
  tool, and it is good at it. Athen's job is to **recognize git state powerfully**
  (read-only) and render it understandably — not to wrap or proxy git.

## 2. Design decisions (settled with the user)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **Per-arc, UI-only flag.** | Senses must stay in the normal posture; coding is a focused, user-initiated session. Mirrors the per-arc `security_mode_override` / `reasoning_effort_override` pattern. |
| D2 | **No worktrees by default.** Default Code Mode works **in-place** on the real repo. | "Separate worktrees by default is stupid" — most coding is serial. Worktrees add cognitive + disk overhead nobody wants for a single agent. |
| D3 | **Worktrees are opt-in, for parallelism.** The agent creates a worktree per parallel sub-agent **only when it fans out**. | This is the one case where a shared tree bites you: parallel agents in one working tree stash/checkout/clobber each other (see `[[feedback_subagents_must_not_run_git]]`). Worktrees are the principled fix at the arc level. |
| D4 | **The agent drives git; Athen recognizes it.** Worktree *creation* is the agent's job via shell git. Athen builds a **read-only recognition layer**. | "The agent can handle git, we just should recognize it very powerfully to offer a great UI/UX." Far lower-risk surface — observation, not control. |
| D5 | **Override the shadow gix snapshot store with the real repo.** When Code Mode + a real repo is detected, the per-action shadow snapshot store is **disabled for that arc**; the Changes/undo surface reads **real git** instead. | A power user wants their real history in their real tools, not a parallel shadow history they can't see. "Override gix with whatever is there." See §6 for the undo-granularity tradeoff (open question). |
| D6 | **Agent visualization = the existing agent-control page, scoped to the arc/project, made live.** | Reuse, don't reinvent. Live sub-agent transcripts shown while running; **only the final output is persisted** (already the delegation contract — sub-agent transcripts are never persisted as first-class history). |
| D7 | **GitHub identity reuses the existing per-profile mechanism.** | `GithubIdentity::{None,Bot,User}` already env-injects `GH_TOKEN` + author/committer + `GH_CONFIG_DIR` into the shell. Code Mode changes nothing here. |

## 3. Architecture overview

```
                 ┌─ per-arc flag: code_mode + code_mode_root (ArcMeta) ─┐
   UI toggle ───►│  set only from desktop/web client, never sense_router │
                 └───────────────────────────────────────────────────────┘
                                       │
              resolve_code_mode_for_arc() in assemble_base_app_tool_registry
                                       │
        ┌──────────────────────────────┼───────────────────────────────┐
        ▼                              ▼                               ▼
  per-arc CWD + fs-base        checkpoint override            git recognition
  (shell + write/edit/read     (skip shadow gix snapshot       layer (read-only
   resolve under repo root,     when real repo present →        `git` observer:
   sandbox allowed-paths        real git is the undo surface)   worktrees, branch,
   widened to repo root)                                        status, diff, log)
        │                                                              │
        └──────────────► executor "Code Mode" static-prefix block ◄────┘
                                       │
                         agent drives git/worktrees via shell
                                       │
        ┌──────────────────────────────┴───────────────────────────────┐
        ▼                                                                ▼
   Code-Mode panel (desktop + web):                          arc-scoped agent
   git/worktree/WIP visualization                            activity (live)
   built on the recognition layer + Changes-rail shape       reuse agent-control
```

## 4. The per-arc flag and working root (§ persistence)

Two new nullable columns on the `arcs` table (`athen-persistence/src/arcs.rs`),
threaded through `ArcMeta` exactly like the recent `project_id` /
`research_paper_path` additions:

- `code_mode INTEGER` (0/1, nullable; `None`/`false` default) — is this arc a
  Code-Mode session?
- `code_mode_root TEXT` (nullable) — absolute path to the repo root the arc
  operates in.

Recipe (the established column-add template):

1. Add `pub code_mode: Option<bool>` + `pub code_mode_root: Option<String>` to
   `ArcMeta` (after `research_question`).
2. Add the two columns to `ARC_SCHEMA_SQL`.
3. Add two PRAGMA-`table_info`-guarded `ALTER TABLE arcs ADD COLUMN …` migrations
   in `init_schema`, reusing the cached-`cols` pattern.
4. Extend **both** SELECT sites in lockstep (`get_arc` + `list_arcs_inner`) with
   the new column indices (27/28) — row-index mismatch panics otherwise.
5. Backfill every `ArcMeta { … }` literal: the two query closures in `arcs.rs`
   plus the **two test fixtures in `athen-app/src/sense_router.rs`** (cross-crate;
   the orchestrator patches these so the persistence change compiles).
6. Setters `set_code_mode(arc_id, Option<bool>)` + `set_code_mode_root(arc_id,
   Option<&str>)`; a `code_mode` round-trip test.

**Resolution default for `code_mode_root`:** when the user enables Code Mode, the
root defaults to the arc's Project folder (`Projects/<slug>/`) if the arc belongs
to a project, else a directory the user picks. Stored as an **absolute** path so
it can point at any existing repo on disk (e.g. an existing checkout outside the
Athen workspace), not only workspace-managed folders.

**Inheritance:** branch arcs inherit the parent's `code_mode` + `code_mode_root`
(so a branched coding session stays a coding session). Sub-arcs spawned by
delegation inherit them via the existing parent-pin propagation in
`delegation.rs`.

## 5. Per-arc working directory + sandbox (§ the load-bearing plumbing)

Today **all arcs share one cwd**: `build_shell_env()` in
`athen-agent/src/tools.rs` sets `cwd = paths::athen_workspace_dir()` globally, and
the file tools (`write`/`edit`/`read`/`save_file`) resolve relative paths via
`paths::resolve_in_workspace`. There is **no per-arc cwd today** — this is the
main new plumbing Code Mode introduces.

Plan (additive, mirrors the existing `checkpoint_arc_id` / `active_project_slug`
threading on `ToolRegistryState`):

- Add `working_dir_override: Option<PathBuf>` to `ToolRegistryState`
  (`athen-agent/src/tools.rs`) with a `with_working_dir(…)` builder.
- `build_shell_env()` uses the override as cwd when present, else
  `athen_workspace_dir()`.
- File tools resolve relative paths against the override when present (an
  `fs_base_override` used by `do_write`/`do_edit`/`do_read`/`save_file`), so a
  shell `ls` and a `write "src/foo.rs"` agree on where "here" is.
- `assemble_base_app_tool_registry()` (`athen-app/src/state.rs`) resolves the
  arc's `code_mode` + `code_mode_root` (new `resolve_code_mode_for_arc`) and calls
  `.with_working_dir(root)` when Code Mode is on.

**Sandbox.** The sandbox's `allowed_paths` currently fences execution to the
workspace. In Code Mode the allowed paths are **widened to include
`code_mode_root`** (the repo root), since the whole point is to operate on a repo
that may live outside the workspace. This is a deliberate, scoped widening:
power-user-only, per-arc, UI-set, explicit directory pick — never reachable from
a sense-created arc. Document it in the security model alongside `SecurityMode`.

## 6. Checkpoint + discard (RESOLVED: option b)

`GixCheckpointStore` snapshots `write`/`edit`/`shell` pre-state into a **shadow**
bare repo at `<data_dir>/athen-snapshots` (branch-per-arc, tag-per-action) and
powers the Changes rail's "Revert this action."

**Decision (option b), confirmed by the user:** the shadow store **stays active
in Code Mode**. Agent actions keep their per-action, file-level undo (the Changes
rail works exactly as in normal arcs). The two undo surfaces are complementary,
not competing:

- **Shadow store → "undo this agent action."** Action-scoped, fires on every
  `write`/`edit`/`shell` even before anything is committed. This is *Athen's*
  edit history, not git's. Build 1 originally skipped it in Code Mode (the old
  D5); Build 2 **reverts that** — `resolve_code_mode_for_arc` now returns only
  the repo root and the checkpoint store is wired unconditionally.
- **Real git → visualization + manual discard.** The Code-Mode panel shows
  branch/worktrees/WIP from the recognition layer, and a **GitLens-style "discard
  changes"** action lets the user throw away working-tree changes git-natively
  (per-file or all).

### Discard changes (git-style)

A user-initiated, UI-only mutating git op (like the Changes-rail Revert button —
**not** agent-callable, **not** risk-gated, but **confirmed** in the UI because it
destroys uncommitted work). Backed by `code_mode_discard_core(arc_id, path)`:

- `path = Some(rel)` → discard one file. If the path is **tracked**, run
  `git -C <root> restore --staged --worktree --source=HEAD -- <rel>` (discards
  staged + unstaged, restores deletes). If **untracked**, `git -C <root> clean
  -fd -- <rel>` (remove it).
- `path = None` → discard **all** working-tree changes: `git -C <root> checkout
  -- .` then `git -C <root> clean -fd` (stronger confirm in the UI).

The path comes from the panel's `dirty[]` list (git-relative) and is **fenced**
to the repo root in code (resolve + reject any path escaping `root`). Returns a
fresh `GitRepoState` so the panel updates in one round-trip. Surfaced as the
`code_mode_discard` Tauri command + `POST /api/arcs/{id}/code-mode/discard`.

## 7. Git recognition layer (read-only)

A new `athen-app/src/code_mode.rs` module exposes a **read-only** observer over
the real repo. It shells out to the system `git` binary directly (not via the
nushell/sandbox agent shell — this is trusted, read-only Athen-internal
observation), parsing porcelain output. This avoids enabling extra `gix` features
(`gix` is pinned `0.83` with only `max-performance-safe, revision, sha1` — no
`worktree`/`status`) and matches "recognize whatever real git is there."

```rust
pub struct GitRepoState {
    pub root: String,
    pub is_repo: bool,
    pub head_branch: Option<String>,   // None when detached
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: Vec<DirtyFile>,          // git status --porcelain=v2
    pub worktrees: Vec<WorktreeInfo>,   // git worktree list --porcelain
    pub recent_commits: Vec<CommitInfo>,// git log -n N
}
pub struct WorktreeInfo { pub path: String, pub branch: Option<String>,
                          pub head: String, pub is_main: bool, pub locked: bool }
```

Commands used (all read-only, `git -C <root> …`): `rev-parse`,
`status --porcelain=v2 --branch`, `worktree list --porcelain`,
`log -n <N> --pretty=…`, `diff --stat`. Missing `git` binary or non-repo root
degrades gracefully to `is_repo: false`.

Exposed as `code_mode_git_state(arc_id)` core fn → Tauri command +
`GET /api/arcs/{id}/code-mode/git` HTTP route. This single reader feeds **both**
the Changes/WIP rail and the worktree lanes in the Code-Mode panel.

## 8. UI — Code-Mode panel + scoped agent activity (desktop + web parity)

**Enabling Code Mode** (per-arc, in both `frontend/` and `web/`):

- A per-arc **Code Mode toggle** (next to the existing per-arc security-mode /
  reasoning pickers) calling `set_arc_code_mode(arc_id, enabled, root)`.
- On enable, a **working-directory picker** (defaults to the Project folder if
  any). Hidden behind a lightweight "developer features" preference so
  non-technical users never see it.

**Code-Mode panel** (new arc-scoped drawer, mirroring the Changes/Wakeups/Agents
drawer shape):

- **Repo header**: root path, current branch (or "detached"), ahead/behind.
- **Worktree lanes**: one row per `git worktree list` entry — path, branch, HEAD,
  WIP diff stat. When the agent fans out parallel sub-agents into worktrees, each
  lane shows that sub-agent's working tree.
- **WIP / Changes**: dirty files + diff stat from the recognition layer (replaces
  the shadow-snapshot Changes rail while Code Mode is on).

**Scoped agent activity** (decision D6): reuse the existing agent-control page
(`#agent-control-view` desktop / `AgentsPanel.tsx` web), **filtered to the current
arc/project**, made live. Sub-agent steps already render via the
`delegate_to_agent` expansion (lazy-loaded sub-arc entries); Code Mode surfaces
them in the panel. Live streaming of in-progress sub-agent transcripts reuses the
Deep-Research progress-event pattern (`*-progress` UiBridge events, arc-scoped
filter); **only final output is persisted** — the live transcript is ephemeral.

## 9. Executor prompt block

When an arc is in Code Mode, the executor injects a small **static-prefix** block
(invariant → cacheable, like `mission_block`/`project_block`) telling the agent:

- It is in a real git repository rooted at `<code_mode_root>`; the shell cwd and
  relative file paths are anchored there.
- It owns git directly (commit, branch, diff) — its commits are the real history.
- For **parallel** work it should create **one `git worktree` per parallel
  sub-agent** (and reuse the delegation fan-out), never run parallel writers in a
  shared tree.
- Athen visualizes worktrees + WIP automatically; it does not need to report git
  state in prose.

## 10. Build phasing

**Build 1 (SHIPPED):** per-arc flag + root (persistence) · per-arc cwd/fs-base +
sandbox widening · git recognition layer + command/route · `[CODE MODE]` prompt
block · UI toggle + dir picker + Code-Mode panel (git/worktree/WIP viz). Build 1
originally skipped the shadow store in Code Mode (the old D5) — superseded below.

**Build 2 (SHIPPED):** undo resolved to **option (b)** — the shadow checkpoint
store stays active in Code Mode (per-action file-level undo via the Changes rail
unchanged); the old D5 skip was reverted (`resolve_code_mode_for_arc` now returns
only the repo root). GitLens-style **discard** (§6): per-file + discard-all,
`git restore`/`clean`, path-fenced, confirm-gated, desktop + web parity.

**Build 3 (future):** live ephemeral sub-agent transcript streaming into the
panel · a generalized `code-mode-progress` event superset · auto-suggesting a
worktree when the agent is about to fan out parallel writers.

## 11. Ordering (file-disjoint waves)

1. **Substrate (parallel):** persistence columns/ArcMeta/setters (`arcs.rs`) +
   git recognition module (`code_mode.rs`, new file). Orchestrator backfills
   `sense_router.rs` ArcMeta fixtures.
2. **athen-agent plumbing:** `working_dir_override` + `fs_base_override` on
   `ToolRegistryState`, `build_shell_env` + file-tool resolution.
3. **athen-app wiring:** `resolve_code_mode_for_arc` + `assemble_base_app_tool_registry`
   gating + sandbox allowed-paths + checkpoint override + executor block;
   `set_arc_code_mode` + `code_mode_git_state` commands + HTTP routes + `lib.rs`.
4. **UI (parallel):** desktop (`frontend/`) and web (`web/src/`) — different files,
   parallel-safe. Rebuild `web/dist`.
5. **Docs + final verify:** `docs/IMPLEMENTATION.md`; full
   `cargo build/clippy --workspace --all-targets -- -D warnings/test`; `cargo fmt
   --all`; commit per wave.

## 12. Risks

- **ArcMeta SELECT sites:** both must change together or get/list panic — add a
  round-trip test (the standing column-add footgun).
- **Per-arc cwd is new:** nothing today threads a non-workspace cwd; the shell +
  file-tool resolution + sandbox allowed-paths must move together or the agent's
  `ls` and `write` will disagree, or the sandbox will deny repo writes.
- **Sandbox widening** to `code_mode_root` is a real expansion of the blast
  radius — keep it strictly gated (UI-only, per-arc, explicit dir, never from
  senses) and documented next to `SecurityMode`.
- **Recognition layer trusts the system `git`** binary; degrade gracefully when
  absent or when the root is not a repo (`is_repo: false`, never panic).
- **Undo override (D5):** disabling shadow snapshots changes what "Revert" means
  — keep (a)/(b) open until confirmed; do not silently drop the user's existing
  Changes-rail expectations without the real-git surface in place.
- **Migrations** stay column-level PRAGMA-guarded; do NOT add a migration
  framework.

## 13. Verification

- `cargo build --workspace && cargo clippy --workspace --all-targets -- -D
  warnings && cargo test --workspace` (the `--all-targets` form is the CI gate;
  the per-crate form misses test-target lints).
- New unit tests: `ArcMeta` code_mode round-trip; `GitRepoState` parsing of
  porcelain fixtures + non-repo graceful path; per-arc cwd resolution.
- Manual (desktop `cargo tauri dev`): enable Code Mode on an arc pointed at a real
  repo → shell `pwd` shows the repo root, `write` lands in the repo, the
  Code-Mode panel shows branch + dirty files; have the agent create a `git
  worktree` → it appears as a lane; confirm sense-created arcs never show the
  toggle as on. Repeat in the web UI for parity.
