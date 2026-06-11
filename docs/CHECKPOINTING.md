# Agent Action Checkpointing & Undo

Hidden git backend that lets the user roll back destructive things Athen did
(deleted files, overwrites, edits). The agent is unaware of the snapshot
layer — it just calls `write`, `edit`, `shell_execute` as usual; the
checkpoint hook fires before each call and records pre-state.

Status: **phase 1 SHIPPED** — `athen-checkpoint` crate is live: structured-tool
snapshots (`write`/`edit`) via `gix`, Changes side panel + point-in-time revert
in both the desktop UI and the web UI. Shell snapshotting comes later.

## Why git, hidden under the hood

Reuses mature, well-debugged machinery for the parts we'd otherwise rebuild:
content-addressed storage, cross-snapshot dedup, packfile compression, atomic
multi-file commits (one commit = one tool action), diff/log/blame tooling,
revert semantics.

Hidden, not surfaced: the user never types a git command. The UI shows
"Revert this action" buttons and a Changes side panel. The repo is an
implementation detail living under `<data_dir>/`. For power users it's an
escape hatch — `cd <data_dir>/athen-snapshots && git log --all` reveals
everything Athen ever changed.

Implementation uses `gix` (gitoxide, pure Rust) — not the `git` subprocess —
to honour CLAUDE.md's "zero runtime deps" rule. We can't assume non-technical
users have `git` installed.

## Storage layout

**One bare repo for the whole app.** Branch-per-arc gives isolation; the
shared object store gives cross-arc dedup automatically (two arcs that both
edit the same `Cargo.toml` share its blob).

```
<data_dir>/athen-snapshots/   ← bare git repo
  ├── HEAD
  ├── objects/                ← shared content-addressed store
  ├── refs/heads/arc/<arc_uuid>      ← one branch per arc
  └── refs/tags/action/<entry_id>    ← one tag per destructive action
```

No working tree. We never check out files. We write blobs directly into
`objects/` and reference them from tree objects whose path structure mirrors
the absolute paths of the original files (`home/alex/.../main.rs`).

## What gets snapshotted

**Lazy, sandbox-fenced, size-capped.**

- **Lazy:** nothing is pre-indexed. Files enter the repo only when a
  destructive action targets them.
- **Sandbox-fenced:** refuse to snapshot any path outside the
  sandbox's `allowed_paths` for the arc that issued the action. We
  wouldn't have written there anyway.
- **Deny-list:** never snapshot system paths even if the sandbox is mis-
  configured: `/etc`, `/usr`, `/proc`, `/sys`, `/var`, `/dev`, `/boot`.
- **Size cap:** per-file ~50 MB. Bigger files are skipped; the arc entry
  records `snapshot: skipped(size)` and the UI shows "Revert unavailable
  (file too large)".
- **Missing files are valid pre-state:** if the agent is about to *create*
  a file, the snapshot records absence by simply not including a blob for
  that path. Revert deletes the path it created.

## Snapshot lifecycle

For each destructive action:

1. **Resolve paths.** Structured tools (`write`/`edit`) supply paths in
   args. Shell tools come from a best-effort parser (see below).
2. **Filter** through sandbox-allow + deny-list + size cap.
3. **Read pre-state.** For each surviving path, read bytes from disk.
   Missing → record absence.
4. **Write blobs.** `repo.write_blob(bytes) → oid` for each present file.
5. **Build tree.** Tree object with path-preserving structure
   (`home/alex/projects/x/main.rs` → blob_oid).
6. **Commit.** Parent = current tip of `refs/heads/arc/<arc_uuid>`,
   message JSON-encodes `{tool_name, args_summary, entry_id, turn_id}`.
7. **Advance branch.** `refs/heads/arc/<arc_uuid>` → new commit.
8. **Tag.** Lightweight tag `refs/tags/action/<entry_id>` → new commit
   (lets us locate a specific action's commit without walking the log).

Tool runs. If it fails, the snapshot stays — harmless, gc'd later.

## Revert primitives

- **Single action:** look up `refs/tags/action/<entry_id>`, diff against
  its parent, apply inverse to the filesystem. Files present in parent
  but absent in target → re-create. Files absent in parent but present
  in target → delete. Modified → restore parent bytes.
- **Bulk arc revert:** reset the filesystem to a chosen earlier commit on
  the arc branch. Walk full diff and apply.
- **List actions in an arc:** read commits on `refs/heads/arc/<uuid>`.
- **Diff rendering:** `gix` patch of any commit → render in side panel.

Revert is itself logged as an arc entry, but **not** snapshotted — that
would build an infinite chain. Re-revert (redo) walks the history.

## Shell-execute parser

Light, whitelist-shape, best-effort. False negatives (no snapshot) are a
degraded-UX failure mode, not a security hole, so we err on the side of
catching common forms cheaply rather than parsing every shell edge case.

Phase-1 recognizer (extend later):
- `rm <paths>`, `rm -r/-rf/-Rf <paths>` → snapshot each path that exists
- `mv <src...> <dst>` → snapshot `dst` if it exists, and each `src`
- `cp <src...> <dst>` → snapshot `dst` if it exists
- `> file`, `>> file`, `tee file`, `tee -a file` → snapshot file if it exists
- `sed -i <flags> <file...>` → snapshot each file
- `touch <file>` → snapshot if exists (no-op otherwise)
- `mkdir`/`rmdir` → snapshot the directory (empty tree = no-op)

**Unrecognized destructive commands** get an arc entry stamped
`snapshot: skipped(unparsed)`. The UI shows "Revert unavailable" for
those, with a tooltip hinting "Athen couldn't pre-snapshot this shell
command — see history for details." This is honest UX: we don't pretend
to undo what we can't.

Long-tail commands we deliberately don't try to handle in phase 1: `git
reset --hard`, `tar -x`, `unzip`, `find -delete`, `xargs rm`, `npm
install` (touches `node_modules/`), arbitrary scripts. They may join the
whitelist later, one at a time, with explicit reasoning.

## UI surface

Two affordances, both fed from the same data:

1. **Per-action "Revert" button** on the tool card in the arc view.
   Available when the entry has a `snapshot_action_id`. One click →
   revert primitive, write a new arc entry recording the revert.
2. **"Changes" side rail** (right panel, toggleable like the existing
   profile/identity rails). Chronological list per arc: icon, paths
   touched, timestamp, revert button. Power-user surface for "what did
   Athen do in this arc."

Both come for free once snapshots are in place — they're just two views
of `list_actions(arc_id)`.

## Retention

Default: **drop the arc's branch + tags when the arc is archived.** Next
`gix gc` reclaims any objects no other branch references. Cross-arc
dedup means popular blobs survive until *every* referencing arc is
archived — which is the right behaviour.

Future knob: a global size cap on `<data_dir>/athen-snapshots/`. When
exceeded, archive the oldest non-archived arcs' branches. Not in phase 1.

## Trait surface (`athen-core::traits::checkpoint`)

```rust
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Snapshot the pre-state of `paths` and commit on the arc's branch.
    /// Returns the action id (= entry_id, echoed for convenience) or
    /// `None` if every path was filtered out (deny-list / size / outside
    /// sandbox).
    async fn snapshot_paths(
        &self,
        arc_id: &str,
        entry_id: &str,
        turn_id: Option<&str>,
        tool_name: &str,
        args_summary: &str,
        paths: &[PathBuf],
    ) -> Result<Option<String>>;

    /// Revert a single action by entry_id. Idempotent (re-running on an
    /// already-reverted action is a no-op).
    async fn revert_action(&self, entry_id: &str) -> Result<RevertOutcome>;

    /// List action records for an arc, newest first.
    async fn list_actions(&self, arc_id: &str) -> Result<Vec<ActionRecord>>;

    /// Drop snapshot history for an archived arc. Idempotent.
    async fn forget_arc(&self, arc_id: &str) -> Result<()>;
}
```

## Crate layout

```
crates/athen-core/src/traits/checkpoint.rs    ← port
crates/athen-checkpoint/                      ← new crate
  ├── Cargo.toml                              ← gix + thiserror + tokio
  └── src/
      ├── lib.rs       ← GixCheckpointStore + open()
      ├── filter.rs    ← deny-list, size cap, sandbox-allow gate
      ├── refs.rs      ← branch/tag naming
      └── shell.rs     ← phase-2 parser (lands later)
```

Trait stays in `athen-core`. Implementation lives in `athen-checkpoint`.
Wired by `athen-app` as the composition root, threaded into the tool
registry's snapshot hook. No sibling-crate dependency.

## Phase plan

1. **Phase 1 — structured snapshots (this PR).** Trait + `athen-checkpoint`
   crate + unit tests. Hook into `write` + `edit` tools. No UI yet.
2. **Phase 2 — shell parser.** Whitelist recognizer + tests. Hook into
   `shell_execute`.
3. **Phase 3 — UI.** Revert button on tool cards + Changes side rail +
   Tauri commands for `list_actions` / `revert_action`.
4. **Phase 4 — retention.** Global size cap, archive-driven `forget_arc`,
   periodic `gix gc`.

Each phase is independently useful and shippable.
