//! Gix-backed implementation of `CheckpointStore`.
//!
//! Single bare repo at `<data_dir>/athen-snapshots`. Branch per arc
//! (`refs/heads/arc/<uuid>`). Lightweight tag per action
//! (`refs/tags/action/<entry_id>`). Cross-arc dedup comes free from
//! git's content-addressed object store.
//!
//! See `docs/CHECKPOINTING.md` for the full design.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::traits::checkpoint::{ActionRecord, CheckpointStore, RevertOutcome};
use tokio::sync::Mutex;

mod filter;
mod refs;
mod tree_builder;

pub use filter::{default_deny_list, CheckpointConfig};

/// Repository-backed `CheckpointStore`.
///
/// Cheap to clone (holds a `gix::ThreadSafeRepository`); a fresh
/// thread-local `Repository` is materialized per operation via
/// `spawn_blocking`. The `write_lock` serialises ref updates so two
/// concurrent snapshots on the same arc don't race the
/// branch-fast-forward.
#[derive(Clone)]
pub struct GixCheckpointStore {
    repo: gix::ThreadSafeRepository,
    write_lock: Arc<Mutex<()>>,
    config: CheckpointConfig,
}

impl GixCheckpointStore {
    /// Open or initialize the bare snapshot repo under `data_dir`.
    /// Idempotent: safe to call on every app start.
    pub fn open(data_dir: &Path) -> Result<Self> {
        Self::open_with_config(data_dir, CheckpointConfig::default())
    }

    pub fn open_with_config(data_dir: &Path, config: CheckpointConfig) -> Result<Self> {
        let repo_path = data_dir.join("athen-snapshots");
        let repo = if repo_path.join("HEAD").exists() {
            gix::open(&repo_path)
                .map_err(|e| AthenError::Other(format!("open snapshot repo: {e}")))?
        } else {
            std::fs::create_dir_all(&repo_path).map_err(AthenError::Io)?;
            gix::init_bare(&repo_path)
                .map_err(|e| AthenError::Other(format!("init snapshot repo: {e}")))?
        };
        Ok(Self {
            repo: repo.into_sync(),
            write_lock: Arc::new(Mutex::new(())),
            config,
        })
    }
}

#[async_trait]
impl CheckpointStore for GixCheckpointStore {
    async fn snapshot_paths(
        &self,
        arc_id: &str,
        entry_id: &str,
        turn_id: Option<&str>,
        tool_name: &str,
        args_summary: &str,
        paths: &[PathBuf],
    ) -> Result<Option<String>> {
        let arc_id = arc_id.to_string();
        let entry_id = entry_id.to_string();
        let turn_id = turn_id.map(|s| s.to_string());
        let tool_name = tool_name.to_string();
        let args_summary = args_summary.to_string();
        let paths = paths.to_vec();
        let store = self.clone();
        let _guard = self.write_lock.lock().await;

        tokio::task::spawn_blocking(move || {
            store.snapshot_paths_sync(
                &arc_id,
                &entry_id,
                turn_id.as_deref(),
                &tool_name,
                &args_summary,
                &paths,
            )
        })
        .await
        .map_err(|e| AthenError::Other(format!("snapshot task join: {e}")))?
    }

    async fn revert_action(&self, entry_id: &str) -> Result<RevertOutcome> {
        let entry_id = entry_id.to_string();
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.revert_action_sync(&entry_id))
            .await
            .map_err(|e| AthenError::Other(format!("revert task join: {e}")))?
    }

    async fn rewind_to_before(&self, arc_id: &str, entry_id: &str) -> Result<RevertOutcome> {
        let arc_id = arc_id.to_string();
        let entry_id = entry_id.to_string();
        let store = self.clone();
        let _guard = self.write_lock.lock().await;
        tokio::task::spawn_blocking(move || store.rewind_to_before_sync(&arc_id, &entry_id))
            .await
            .map_err(|e| AthenError::Other(format!("rewind task join: {e}")))?
    }

    async fn list_actions(&self, arc_id: &str) -> Result<Vec<ActionRecord>> {
        let arc_id = arc_id.to_string();
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.list_actions_sync(&arc_id))
            .await
            .map_err(|e| AthenError::Other(format!("list task join: {e}")))?
    }

    async fn forget_arc(&self, arc_id: &str) -> Result<()> {
        let arc_id = arc_id.to_string();
        let store = self.clone();
        let _guard = self.write_lock.lock().await;
        tokio::task::spawn_blocking(move || store.forget_arc_sync(&arc_id))
            .await
            .map_err(|e| AthenError::Other(format!("forget task join: {e}")))?
    }
}

// ---------- Sync internals (run on blocking pool) ----------

/// JSON payload stored in the commit message so we don't have to
/// re-derive metadata from the tool args at revert time.
#[derive(serde::Serialize, serde::Deserialize)]
struct CommitMeta {
    entry_id: String,
    turn_id: Option<String>,
    arc_id: String,
    tool_name: String,
    args_summary: String,
    /// True for paths that were absent at snapshot time (file did not
    /// exist before the action). Stored alongside the tree so revert
    /// knows to delete-on-revert rather than restore.
    absent_paths: Vec<PathBuf>,
}

impl GixCheckpointStore {
    fn snapshot_paths_sync(
        &self,
        arc_id: &str,
        entry_id: &str,
        turn_id: Option<&str>,
        tool_name: &str,
        args_summary: &str,
        paths: &[PathBuf],
    ) -> Result<Option<String>> {
        let repo = self.repo.to_thread_local();

        // Filter: deny-list, size, dedup. Sandbox-allow is upstream of
        // us (the tool registry should not call us for paths it
        // wouldn't have written), so we don't reapply it here.
        let kept: Vec<PathBuf> = filter::keep_snapshottable(paths, &self.config);
        if kept.is_empty() {
            return Ok(None);
        }

        // Read pre-state for each kept path. Absent files are tracked
        // separately so revert can re-delete them.
        let mut present: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut absent: Vec<PathBuf> = Vec::new();
        for p in &kept {
            match std::fs::read(p) {
                Ok(bytes) => {
                    present.insert(p.clone(), bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    absent.push(p.clone());
                }
                Err(e) => {
                    // Permission denied etc. — record as absent so revert
                    // won't try to restore something we couldn't read.
                    tracing::warn!(path = %p.display(), error = %e, "snapshot: read failed, recording as absent");
                    absent.push(p.clone());
                }
            }
        }

        // Edge case: every path filtered to absent + filtered set was
        // already empty -> nothing meaningful to commit. We still
        // commit absent-only sets so revert can delete-on-revert.
        if present.is_empty() && absent.is_empty() {
            return Ok(None);
        }

        // Write blobs. Path keys are normalised relative to the
        // filesystem root (leading `/` stripped, Windows drive letter
        // mapped to a single-letter component). Same shape used for
        // tree paths and for revert lookup.
        let mut path_to_oid: BTreeMap<PathBuf, gix::ObjectId> = BTreeMap::new();
        for (p, bytes) in &present {
            let oid = repo
                .write_blob(bytes)
                .map_err(|e| AthenError::Other(format!("write_blob {}: {e}", p.display())))?
                .detach();
            let rel = refs::strip_root(p);
            path_to_oid.insert(rel, oid);
        }

        // Build nested tree from the flat path→blob map.
        let tree_oid = tree_builder::write_tree(&repo, &path_to_oid)?;

        // Parent is the current tip of the arc branch (if any).
        let branch = refs::arc_branch(arc_id);
        let parent = repo
            .find_reference(&branch)
            .ok()
            .and_then(|mut r| r.peel_to_id_in_place().ok())
            .map(|id| id.detach());

        let meta = CommitMeta {
            entry_id: entry_id.to_string(),
            turn_id: turn_id.map(|s| s.to_string()),
            arc_id: arc_id.to_string(),
            tool_name: tool_name.to_string(),
            args_summary: args_summary.to_string(),
            absent_paths: absent.iter().map(|p| refs::strip_root(p)).collect(),
        };
        let message = serde_json::to_string(&meta)
            .map_err(|e| AthenError::Other(format!("encode commit meta: {e}")))?;

        let now = chrono::Utc::now();
        let signature = gix::actor::SignatureRef {
            name: "athen-checkpoint".into(),
            email: "athen@localhost".into(),
            time: gix::date::Time::new(now.timestamp() as gix::date::SecondsSinceUnixEpoch, 0),
        };
        let commit = gix::objs::Commit {
            tree: tree_oid,
            parents: parent.iter().copied().collect(),
            author: signature.into(),
            committer: signature.into(),
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_oid = repo
            .write_object(&commit)
            .map_err(|e| AthenError::Other(format!("write commit: {e}")))?
            .detach();

        // Advance the arc branch and tag the commit by entry_id.
        repo.reference(
            branch.as_str(),
            commit_oid,
            gix::refs::transaction::PreviousValue::Any,
            format!("checkpoint: {tool_name} ({entry_id})"),
        )
        .map_err(|e| AthenError::Other(format!("update arc branch: {e}")))?;

        let tag = refs::action_tag(entry_id);
        repo.reference(
            tag.as_str(),
            commit_oid,
            gix::refs::transaction::PreviousValue::Any,
            "checkpoint action tag",
        )
        .map_err(|e| AthenError::Other(format!("create action tag: {e}")))?;

        Ok(Some(entry_id.to_string()))
    }

    fn revert_action_sync(&self, entry_id: &str) -> Result<RevertOutcome> {
        let repo = self.repo.to_thread_local();
        let tag = refs::action_tag(entry_id);
        let mut tag_ref = match repo.find_reference(&tag) {
            Ok(r) => r,
            Err(_) => return Ok(RevertOutcome::default()), // nothing to revert
        };
        let commit_id = tag_ref
            .peel_to_id_in_place()
            .map_err(|e| AthenError::Other(format!("peel tag {tag}: {e}")))?
            .detach();
        let commit = repo
            .find_object(commit_id)
            .map_err(|e| AthenError::Other(format!("find commit {commit_id}: {e}")))?
            .into_commit();
        let meta_text = commit
            .message_raw()
            .map_err(|e| AthenError::Other(format!("read commit message: {e}")))?;
        let meta: CommitMeta = serde_json::from_slice(meta_text)
            .map_err(|e| AthenError::Other(format!("decode commit meta: {e}")))?;
        let snapshot_tree_id = commit
            .tree_id()
            .map_err(|e| AthenError::Other(format!("read commit tree: {e}")))?
            .detach();

        // Read all (path, blob_oid) pairs out of the snapshot tree.
        let snapshot_entries = tree_builder::flatten_tree(&repo, snapshot_tree_id)?;

        let mut outcome = RevertOutcome::default();

        // Restore each path that has a blob (file existed pre-action).
        for (rel_path, blob_oid) in &snapshot_entries {
            let abs_path = refs::abs_from_rel(rel_path);
            let existed_before_revert = abs_path.exists();
            let bytes = match repo.find_object(*blob_oid) {
                Ok(o) => o.into_blob().data.clone(),
                Err(e) => {
                    outcome
                        .failed
                        .push((abs_path.clone(), format!("read blob: {e}")));
                    continue;
                }
            };
            if let Some(parent) = abs_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    outcome
                        .failed
                        .push((abs_path.clone(), format!("mkdir parent: {e}")));
                    continue;
                }
            }
            match std::fs::write(&abs_path, &bytes) {
                Ok(()) => {
                    if existed_before_revert {
                        outcome.restored.push(abs_path);
                    } else {
                        outcome.recreated.push(abs_path);
                    }
                }
                Err(e) => outcome.failed.push((abs_path, format!("write file: {e}"))),
            }
        }

        // Delete each path that was absent pre-action (= the action
        // created the file).
        for rel_path in &meta.absent_paths {
            let abs_path = refs::abs_from_rel(rel_path);
            if !abs_path.exists() {
                continue;
            }
            match std::fs::remove_file(&abs_path) {
                Ok(()) => outcome.deleted.push(abs_path),
                Err(e) => outcome.failed.push((abs_path, format!("delete file: {e}"))),
            }
        }

        Ok(outcome)
    }

    /// Walk the arc branch from HEAD backward, restoring each commit's
    /// pre-state until (and including) the commit tagged `entry_id`.
    /// Then reset the branch HEAD to that commit's parent (or delete
    /// the branch if no parent) and drop the matching tags.
    fn rewind_to_before_sync(&self, arc_id: &str, entry_id: &str) -> Result<RevertOutcome> {
        let repo = self.repo.to_thread_local();
        let branch = refs::arc_branch(arc_id);
        let mut branch_ref = match repo.find_reference(&branch) {
            Ok(r) => r,
            Err(_) => return Ok(RevertOutcome::default()), // no branch -> nothing to do
        };
        let head_id = branch_ref
            .peel_to_id_in_place()
            .map_err(|e| AthenError::Other(format!("peel branch {branch}: {e}")))?
            .detach();

        // Collect commits newest-first from HEAD until we encounter the
        // one whose meta.entry_id matches. We capture each commit's id,
        // its parent (the rewind target if we land here), and its
        // entry_id (for tag cleanup).
        struct Stop {
            new_head: Option<gix::ObjectId>,
        }
        let mut entry_ids_to_drop: Vec<String> = Vec::new();
        let mut stop: Option<Stop> = None;

        let walk = repo
            .rev_walk([head_id])
            .all()
            .map_err(|e| AthenError::Other(format!("rev walk: {e}")))?;
        for info in walk {
            let info = info.map_err(|e| AthenError::Other(format!("walk step: {e}")))?;
            let commit = repo
                .find_object(info.id)
                .map_err(|e| AthenError::Other(format!("find commit {}: {e}", info.id)))?
                .into_commit();
            let raw = commit
                .message_raw()
                .map_err(|e| AthenError::Other(format!("read message: {e}")))?;
            let meta: CommitMeta = match serde_json::from_slice(raw) {
                Ok(m) => m,
                Err(_) => continue, // foreign commit — skip but keep collecting
            };
            entry_ids_to_drop.push(meta.entry_id.clone());
            if meta.entry_id == entry_id {
                // First parent (if any) is the new branch HEAD.
                let parent = commit.parent_ids().next().map(|p| p.detach());
                stop = Some(Stop { new_head: parent });
                break;
            }
        }

        let Some(Stop { new_head }) = stop else {
            // entry_id not found on this branch -> idempotent no-op.
            return Ok(RevertOutcome::default());
        };

        // Restore files newest-first. We replay each commit's pre-state
        // (the snapshot tree + absent_paths list); when an older commit
        // also covers a path written by a newer one, the older write
        // wins, leaving the file at the pre-`entry_id` state.
        let mut outcome = RevertOutcome::default();
        for eid in &entry_ids_to_drop {
            let single = self.revert_action_sync(eid)?;
            outcome.restored.extend(single.restored);
            outcome.recreated.extend(single.recreated);
            outcome.deleted.extend(single.deleted);
            outcome.failed.extend(single.failed);
        }

        // Reset the branch HEAD to entry_id's parent, or delete the
        // branch if no parent (entry_id was the first action).
        match new_head {
            Some(parent_id) => {
                repo.reference(
                    branch.as_str(),
                    parent_id,
                    gix::refs::transaction::PreviousValue::Any,
                    format!("rewind before {entry_id}"),
                )
                .map_err(|e| AthenError::Other(format!("reset arc branch: {e}")))?;
            }
            None => {
                if let Ok(b) = repo.find_reference(&branch) {
                    let _ = b.delete();
                }
            }
        }

        // Drop the action tags. Best-effort: a missing tag is fine.
        let discarded = entry_ids_to_drop.len();
        for eid in &entry_ids_to_drop {
            let tag = refs::action_tag(eid);
            if let Ok(tag_ref) = repo.find_reference(&tag) {
                let _ = tag_ref.delete();
            }
        }
        outcome.discarded = discarded;

        Ok(outcome)
    }

    fn list_actions_sync(&self, arc_id: &str) -> Result<Vec<ActionRecord>> {
        let repo = self.repo.to_thread_local();
        let branch = refs::arc_branch(arc_id);
        let mut branch_ref = match repo.find_reference(&branch) {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };
        let head_id = branch_ref
            .peel_to_id_in_place()
            .map_err(|e| AthenError::Other(format!("peel branch {branch}: {e}")))?
            .detach();

        let mut out = Vec::new();
        let walk = repo
            .rev_walk([head_id])
            .all()
            .map_err(|e| AthenError::Other(format!("rev walk: {e}")))?;
        for info in walk {
            let info = info.map_err(|e| AthenError::Other(format!("walk step: {e}")))?;
            let commit = repo
                .find_object(info.id)
                .map_err(|e| AthenError::Other(format!("find commit {}: {e}", info.id)))?
                .into_commit();
            let raw = commit
                .message_raw()
                .map_err(|e| AthenError::Other(format!("read message: {e}")))?;
            let meta: CommitMeta = match serde_json::from_slice(raw) {
                Ok(m) => m,
                Err(_) => continue, // skip foreign commits (shouldn't exist, but harmless)
            };
            let snapshot_tree_id = commit
                .tree_id()
                .map_err(|e| AthenError::Other(format!("read tree: {e}")))?
                .detach();
            let present_paths: Vec<PathBuf> = tree_builder::flatten_tree(&repo, snapshot_tree_id)?
                .into_iter()
                .map(|(p, _)| refs::abs_from_rel(&p))
                .collect();
            let mut paths = present_paths;
            paths.extend(meta.absent_paths.iter().map(|p| refs::abs_from_rel(p)));

            let secs = commit
                .time()
                .map(|t| t.seconds)
                .map_err(|e| AthenError::Other(format!("read commit time: {e}")))?;
            let created_at = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                .unwrap_or_else(chrono::Utc::now);

            // `reverted` is determined by walking forward and looking
            // for a same-arc commit whose meta marks this entry as
            // reverted. Phase 1 doesn't write those yet, so this stays
            // false. Wire-up lands when the UI revert command writes a
            // marker entry.
            out.push(ActionRecord {
                entry_id: meta.entry_id,
                turn_id: meta.turn_id,
                tool_name: meta.tool_name,
                args_summary: meta.args_summary,
                created_at,
                paths,
                reverted: false,
            });
        }

        Ok(out)
    }

    fn forget_arc_sync(&self, arc_id: &str) -> Result<()> {
        let repo = self.repo.to_thread_local();
        let branch = refs::arc_branch(arc_id);

        // Collect entry_ids referenced by this branch so we can delete
        // their tags too. Done before deleting the branch (afterwards
        // the commits may become unreachable).
        let mut tags_to_drop: Vec<String> = Vec::new();
        if let Ok(mut branch_ref) = repo.find_reference(&branch) {
            if let Ok(head_id) = branch_ref.peel_to_id_in_place() {
                let head_id = head_id.detach();
                if let Ok(walk) = repo.rev_walk([head_id]).all() {
                    for info in walk.flatten() {
                        if let Ok(commit) = repo.find_object(info.id).map(|o| o.into_commit()) {
                            if let Ok(raw) = commit.message_raw() {
                                if let Ok(meta) = serde_json::from_slice::<CommitMeta>(raw) {
                                    tags_to_drop.push(refs::action_tag(&meta.entry_id));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Delete the branch first, then the tags.
        if let Ok(branch_ref) = repo.find_reference(&branch) {
            let _ = branch_ref.delete();
        }
        for tag in tags_to_drop {
            if let Ok(tag_ref) = repo.find_reference(&tag) {
                let _ = tag_ref.delete();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store() -> (GixCheckpointStore, TempDir) {
        let data = TempDir::new().unwrap();
        let store = GixCheckpointStore::open(data.path()).unwrap();
        (store, data)
    }

    #[tokio::test]
    async fn snapshot_and_revert_modify_round_trip() {
        let work = TempDir::new().unwrap();
        let file = work.path().join("hello.txt");
        std::fs::write(&file, b"v1").unwrap();

        let (store, _data) = make_store();
        let entry_id = store
            .snapshot_paths(
                "arc-a",
                "entry-1",
                None,
                "write",
                "write hello.txt",
                std::slice::from_ref(&file),
            )
            .await
            .unwrap();
        assert_eq!(entry_id.as_deref(), Some("entry-1"));

        // Agent "writes" new content over the file.
        std::fs::write(&file, b"v2-mutated").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"v2-mutated");

        let outcome = store.revert_action("entry-1").await.unwrap();
        assert_eq!(outcome.restored.len(), 1);
        assert_eq!(std::fs::read(&file).unwrap(), b"v1");
    }

    #[tokio::test]
    async fn snapshot_and_revert_create_deletes_file() {
        let work = TempDir::new().unwrap();
        let file = work.path().join("brand-new.txt");
        // File does not exist yet — the "action" would create it.

        let (store, _data) = make_store();
        store
            .snapshot_paths(
                "arc-b",
                "entry-1",
                None,
                "write",
                "write brand-new.txt",
                std::slice::from_ref(&file),
            )
            .await
            .unwrap();

        std::fs::write(&file, b"created-by-agent").unwrap();
        assert!(file.exists());

        let outcome = store.revert_action("entry-1").await.unwrap();
        assert_eq!(outcome.deleted.len(), 1);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn list_actions_returns_newest_first() {
        let work = TempDir::new().unwrap();
        let f1 = work.path().join("a.txt");
        let f2 = work.path().join("b.txt");
        std::fs::write(&f1, b"a").unwrap();
        std::fs::write(&f2, b"b").unwrap();

        let (store, _data) = make_store();
        store
            .snapshot_paths(
                "arc-c",
                "e1",
                None,
                "write",
                "write a.txt",
                std::slice::from_ref(&f1),
            )
            .await
            .unwrap();
        store
            .snapshot_paths(
                "arc-c",
                "e2",
                None,
                "edit",
                "edit b.txt",
                std::slice::from_ref(&f2),
            )
            .await
            .unwrap();

        let actions = store.list_actions("arc-c").await.unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].entry_id, "e2");
        assert_eq!(actions[1].entry_id, "e1");
    }

    #[tokio::test]
    async fn forget_arc_drops_branch_and_tags() {
        let work = TempDir::new().unwrap();
        let f = work.path().join("z.txt");
        std::fs::write(&f, b"z").unwrap();

        let (store, _data) = make_store();
        store
            .snapshot_paths(
                "arc-d",
                "ez",
                None,
                "write",
                "write z.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        assert_eq!(store.list_actions("arc-d").await.unwrap().len(), 1);

        store.forget_arc("arc-d").await.unwrap();
        assert!(store.list_actions("arc-d").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_paths_returns_none() {
        let (store, _data) = make_store();
        let result = store
            .snapshot_paths("arc-e", "entry-empty", None, "write", "noop", &[])
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn rewind_restores_files_and_drops_history() {
        let work = TempDir::new().unwrap();
        let f = work.path().join("doc.txt");
        std::fs::write(&f, b"v1").unwrap();

        let (store, _data) = make_store();
        // Action 1: write — pre-state = v1 (file exists).
        store
            .snapshot_paths(
                "arc-r",
                "e1",
                None,
                "write",
                "write doc.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        std::fs::write(&f, b"v2").unwrap();

        // Action 2: edit — pre-state = v2.
        store
            .snapshot_paths(
                "arc-r",
                "e2",
                None,
                "edit",
                "edit doc.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        std::fs::write(&f, b"v3").unwrap();

        // Action 3: edit again — pre-state = v3.
        store
            .snapshot_paths(
                "arc-r",
                "e3",
                None,
                "edit",
                "edit doc.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        std::fs::write(&f, b"v4").unwrap();

        assert_eq!(store.list_actions("arc-r").await.unwrap().len(), 3);

        // Rewind to before e2: undo e2 + e3. File lands at e2's pre-state
        // (= v2, what existed just before e2 ran), and only e1 stays in
        // history.
        let outcome = store.rewind_to_before("arc-r", "e2").await.unwrap();
        assert_eq!(outcome.discarded, 2);
        assert_eq!(std::fs::read(&f).unwrap(), b"v2");

        let remaining = store.list_actions("arc-r").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].entry_id, "e1");
    }

    #[tokio::test]
    async fn rewind_to_first_action_deletes_branch() {
        let work = TempDir::new().unwrap();
        let f = work.path().join("only.txt");
        // File does not exist — action will create it.

        let (store, _data) = make_store();
        store
            .snapshot_paths(
                "arc-q",
                "only",
                None,
                "write",
                "write only.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        std::fs::write(&f, b"created").unwrap();

        let outcome = store.rewind_to_before("arc-q", "only").await.unwrap();
        assert_eq!(outcome.discarded, 1);
        assert_eq!(outcome.deleted.len(), 1);
        assert!(!f.exists());
        assert!(store.list_actions("arc-q").await.unwrap().is_empty());

        // Branch is gone — a fresh snapshot on the same arc still works.
        std::fs::write(&f, b"fresh").unwrap();
        store
            .snapshot_paths(
                "arc-q",
                "fresh",
                None,
                "write",
                "rewrite",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();
        assert_eq!(store.list_actions("arc-q").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rewind_unknown_entry_id_is_noop() {
        let work = TempDir::new().unwrap();
        let f = work.path().join("x.txt");
        std::fs::write(&f, b"x").unwrap();

        let (store, _data) = make_store();
        store
            .snapshot_paths(
                "arc-z",
                "e1",
                None,
                "write",
                "write x.txt",
                std::slice::from_ref(&f),
            )
            .await
            .unwrap();

        let outcome = store
            .rewind_to_before("arc-z", "nonexistent")
            .await
            .unwrap();
        assert_eq!(outcome.discarded, 0);
        assert!(outcome.restored.is_empty());
        assert_eq!(store.list_actions("arc-z").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cross_arc_blob_dedup() {
        // Two arcs snapshot the same file — same bytes should produce
        // the same blob oid in git's object store. Verified indirectly:
        // both reverts succeed and produce identical content.
        let work = TempDir::new().unwrap();
        let f = work.path().join("shared.txt");
        std::fs::write(&f, b"shared-bytes").unwrap();

        let (store, _data) = make_store();
        store
            .snapshot_paths("arc-x", "e-x", None, "write", "x", std::slice::from_ref(&f))
            .await
            .unwrap();
        store
            .snapshot_paths("arc-y", "e-y", None, "write", "y", std::slice::from_ref(&f))
            .await
            .unwrap();

        std::fs::write(&f, b"mutated").unwrap();

        store.revert_action("e-x").await.unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"shared-bytes");
        std::fs::write(&f, b"mutated-again").unwrap();
        store.revert_action("e-y").await.unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"shared-bytes");
    }
}
