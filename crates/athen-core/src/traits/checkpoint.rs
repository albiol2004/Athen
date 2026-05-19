//! File-snapshot port for agent action undo.
//!
//! Implementations live in `athen-checkpoint` (gix-backed bare git repo).
//! See `docs/CHECKPOINTING.md` for the storage model — one bare repo
//! shared across the app, one branch per arc, one tag per action,
//! cross-arc blob dedup for free.
//!
//! The agent never sees this layer. The tool registry calls
//! `snapshot_paths` before each destructive tool invocation; the UI
//! later calls `revert_action` when the user clicks Revert.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One recorded action with the metadata the UI needs to render its row
/// and decide whether the Revert button is available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Echo of the arc entry that produced this action. Stable key the
    /// UI uses to locate the corresponding tool card.
    pub entry_id: String,
    pub turn_id: Option<String>,
    pub tool_name: String,
    /// Short human-readable summary of the tool args (truncated). The
    /// real args live on the arc entry — this is for the Changes rail.
    pub args_summary: String,
    /// Wall-clock time of the commit author timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Paths touched by this action (relative to filesystem root, with
    /// leading `/` stripped — same shape as the in-tree path).
    pub paths: Vec<PathBuf>,
    /// True once a successful `revert_action(entry_id)` has run. Set so
    /// the UI can grey out the button without losing the history row.
    pub reverted: bool,
}

/// Outcome of a revert. Returned so the UI can show "restored N files,
/// recreated M, deleted K" without having to diff the filesystem itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevertOutcome {
    pub restored: Vec<PathBuf>,
    pub recreated: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    /// Paths that couldn't be reverted (e.g. permission denied, parent
    /// directory missing and uncreatable). Non-fatal — best-effort
    /// revert proceeds with the rest.
    pub failed: Vec<(PathBuf, String)>,
    /// Number of action records dropped from history during the
    /// operation. `revert_action` always leaves this at 0;
    /// `rewind_to_before` reports how many records it discarded.
    #[serde(default)]
    pub discarded: usize,
}

/// Hidden file-snapshot store. See module-level docs.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Snapshot the pre-state of `paths` and commit it on the arc's
    /// branch. Each path is filtered through the implementation's
    /// allow/deny/size gates; the commit only carries paths that
    /// survived.
    ///
    /// Returns `Some(entry_id)` when at least one path was snapshotted,
    /// `None` when every path was filtered out (in which case the
    /// caller should stamp `snapshot: skipped` on the arc entry).
    /// `entry_id` is echoed back as a convenience for the caller.
    ///
    /// `args_summary` is a short, already-truncated human-readable
    /// hint stored on the commit so the Changes rail can render rows
    /// without rehydrating the full arc entry.
    async fn snapshot_paths(
        &self,
        arc_id: &str,
        entry_id: &str,
        turn_id: Option<&str>,
        tool_name: &str,
        args_summary: &str,
        paths: &[PathBuf],
    ) -> Result<Option<String>>;

    /// Revert a single action by `entry_id`. Idempotent: a second call
    /// on an already-reverted action is a no-op that still returns
    /// success with an empty `RevertOutcome`.
    async fn revert_action(&self, entry_id: &str) -> Result<RevertOutcome>;

    /// Rewind the arc to just before `entry_id`'s action: restore
    /// filesystem state to entry_id's pre-state AND drop entry_id plus
    /// every newer action from history (tag + commit chain trimmed).
    ///
    /// Restoration walks newest-first through the discarded actions so
    /// later actions don't overwrite earlier restorations. The arc
    /// branch HEAD is reset to entry_id's parent commit (or the branch
    /// is deleted if entry_id was the first action). Idempotent: if
    /// `entry_id` is unknown, returns an empty outcome with
    /// `discarded = 0`.
    async fn rewind_to_before(&self, arc_id: &str, entry_id: &str) -> Result<RevertOutcome>;

    /// List action records for an arc, newest first. Returns an empty
    /// vec for arcs with no snapshotted actions.
    async fn list_actions(&self, arc_id: &str) -> Result<Vec<ActionRecord>>;

    /// Drop snapshot history for an archived arc. Idempotent. Frees the
    /// branch + tags; orphaned objects are reclaimed by the next gc.
    async fn forget_arc(&self, arc_id: &str) -> Result<()>;
}
