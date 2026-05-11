//! Persistent record of `shell_spawn`'d PIDs at `<data_dir>/spawned_pids.json`.
//!
//! Survives crashes/power-loss so a fresh app start can hunt down any
//! orphaned children of a previous run before it spawns new ones. Without
//! this, persistent wake-ups would happily re-fire shells on top of an
//! ever-growing pile of zombies.
//!
//! Wired into the in-memory [`athen_agent::SpawnedProcessMap`] via the
//! [`athen_agent::SpawnPersistenceHook`] trait — every insert / remove /
//! bulk-clear triggers an atomic rewrite of the JSON file. On startup
//! `reconcile_orphans` reads the file, best-effort kills each PID, and
//! truncates the file.
//!
//! False-positives from PID reuse are tolerated: we'd rather kill an
//! unrelated process briefly than leave a leaked watcher pinning the
//! bundled `nu.exe` after a crash. The window is at most one run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use athen_agent::{SpawnPersistenceHook, SpawnedProcess};

/// On-disk shape of a tracked spawn. Mirrors [`SpawnedProcess`] minus
/// the log-path field (which is interesting to the agent for reads
/// but not load-bearing for reconciliation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PidFileEntry {
    pub pid: u32,
    pub command: String,
    pub label: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// Resolve the pidfile path under the app data dir.
pub fn pidfile_path(data_dir: &Path) -> PathBuf {
    data_dir.join("spawned_pids.json")
}

/// Atomically rewrite the pidfile to contain exactly `entries`.
///
/// Writes to `<path>.tmp` first then renames so a crash mid-write can't
/// leave a half-flushed file. Best-effort: every IO failure is logged
/// but never propagated — a flaky disk shouldn't break `shell_spawn`.
pub async fn write_pidfile(path: &Path, entries: &[PidFileEntry]) {
    let json = match serde_json::to_vec_pretty(entries) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "spawned_pids.json: serialize failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = tokio::fs::write(&tmp, &json).await {
        tracing::warn!(
            path = %tmp.display(),
            error = %e,
            "spawned_pids.json: temp write failed"
        );
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        tracing::warn!(
            from = %tmp.display(),
            to = %path.display(),
            error = %e,
            "spawned_pids.json: rename failed"
        );
        // Best-effort cleanup of the orphan tmp.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

/// Load the pidfile. Returns `Vec::new()` on any failure (missing,
/// garbage, IO error). Same best-effort posture as [`write_pidfile`].
pub async fn read_pidfile(path: &Path) -> Vec<PidFileEntry> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "spawned_pids.json: read failed"
            );
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Vec<PidFileEntry>>(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "spawned_pids.json: parse failed; treating as empty"
            );
            Vec::new()
        }
    }
}

/// Best-effort kill every PID recorded in the file, then truncate it.
/// Returns the number of entries we attempted to kill (not necessarily
/// the number actually alive — there's no cheap cross-platform liveness
/// probe and PID reuse can race us either direction).
pub async fn reconcile_orphans(path: &Path) -> usize {
    let entries = read_pidfile(path).await;
    if entries.is_empty() {
        return 0;
    }
    tracing::info!(
        count = entries.len(),
        "Reconciling orphaned spawned processes from previous run"
    );
    for e in &entries {
        tracing::info!(pid = e.pid, command = %e.command, "Killing orphan");
        athen_agent::kill_spawned_pid(e.pid).await;
    }
    // Truncate so a second startup doesn't try to re-kill the same set.
    write_pidfile(path, &[]).await;
    entries.len()
}

/// Pidfile-backed implementation of [`SpawnPersistenceHook`]. Each
/// snapshot rewrite is atomic (tmp+rename); failures are logged, not
/// propagated.
pub struct PidFilePersistence {
    path: PathBuf,
}

impl PidFilePersistence {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path })
    }
}

#[async_trait]
impl SpawnPersistenceHook for PidFilePersistence {
    async fn on_change(&self, snapshot: Vec<SpawnedProcess>) {
        let entries: Vec<PidFileEntry> = snapshot
            .into_iter()
            .map(|p| PidFileEntry {
                pid: p.pid,
                command: p.command,
                label: p.label,
                started_at: p.started_at,
            })
            .collect();
        write_pidfile(&self.path, &entries).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempfile(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "athen-spawn-pidfile-test-{}-{}",
            uuid::Uuid::new_v4(),
            name
        ));
        p
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let path = tempfile("roundtrip.json");
        let entries = vec![
            PidFileEntry {
                pid: 42,
                command: "sleep 10".to_string(),
                label: Some("test-a".to_string()),
                started_at: Utc::now(),
            },
            PidFileEntry {
                pid: 4242,
                command: "tail -f /var/log/nothing".to_string(),
                label: None,
                started_at: Utc::now(),
            },
        ];
        write_pidfile(&path, &entries).await;
        let loaded = read_pidfile(&path).await;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].pid, 42);
        assert_eq!(loaded[0].command, "sleep 10");
        assert_eq!(loaded[0].label.as_deref(), Some("test-a"));
        assert_eq!(loaded[1].pid, 4242);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_missing_returns_empty() {
        let path = tempfile("missing.json");
        // Don't write anything.
        let loaded = read_pidfile(&path).await;
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn read_garbage_returns_empty() {
        let path = tempfile("garbage.json");
        tokio::fs::write(&path, b"this is not JSON {{{")
            .await
            .unwrap();
        let loaded = read_pidfile(&path).await;
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn persistence_hook_writes_snapshot() {
        let path = tempfile("hook.json");
        let hook = PidFilePersistence::new(path.clone());
        let snapshot = vec![SpawnedProcess {
            pid: 123,
            command: "sleep 1".to_string(),
            label: Some("hook-test".to_string()),
            log_path: PathBuf::from("/tmp/whatever.log"),
            started_at: Utc::now(),
        }];
        hook.on_change(snapshot).await;

        let loaded = read_pidfile(&path).await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pid, 123);
        assert_eq!(loaded[0].command, "sleep 1");
        assert_eq!(loaded[0].label.as_deref(), Some("hook-test"));

        // Now overwrite with empty — confirms the rewrite path truncates.
        hook.on_change(Vec::new()).await;
        let after_clear = read_pidfile(&path).await;
        assert!(after_clear.is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
