//! Path filtering: deny-list, size cap, dedup.
//!
//! The sandbox `allowed_paths` gate is enforced *upstream* — by the
//! tool registry that decides whether to call us at all. This module
//! only enforces the hard floor: never snapshot system paths, never
//! snapshot huge files, and never include the same path twice in one
//! commit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Per-store policy knobs.
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    /// Files larger than this on disk are skipped. The arc entry that
    /// produced the action should record `snapshot: skipped(size)` so
    /// the UI can show "Revert unavailable (file too large)".
    pub max_file_bytes: u64,
    /// Path prefixes that are never snapshotted regardless of sandbox
    /// allow-list. System dirs, runtime state, etc.
    pub deny_prefixes: Vec<PathBuf>,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 50 * 1024 * 1024,
            deny_prefixes: default_deny_list(),
        }
    }
}

/// Hard deny-list applied even when the sandbox would have permitted
/// the write. These paths either contain secrets, are virtual
/// filesystems, or have no meaningful "revert" semantics.
pub fn default_deny_list() -> Vec<PathBuf> {
    [
        "/etc", "/usr", "/proc", "/sys", "/var", "/dev", "/boot", "/run",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Apply deny-list + size cap + dedup. Returns the surviving paths in
/// input order (de-duplicated). Files that don't exist are kept — the
/// snapshot stores them as `absent_paths` so revert can re-delete.
pub fn keep_snapshottable(paths: &[PathBuf], cfg: &CheckpointConfig) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for raw in paths {
        let abs = match raw.canonicalize() {
            Ok(p) => p,
            // canonicalize fails on missing files. We still want to
            // snapshot "this file will be created" — fall back to the
            // input path. We *do* lose symlink resolution, but that's
            // acceptable for non-existent destinations.
            Err(_) => raw.clone(),
        };
        if is_denied(&abs, &cfg.deny_prefixes) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&abs) {
            if meta.is_dir() {
                // Phase 1: skip directories. They're not the common
                // case for `write`/`edit`, and shell-execute parser is
                // out of scope for now.
                continue;
            }
            if meta.len() > cfg.max_file_bytes {
                continue;
            }
        }
        if seen.insert(abs.clone()) {
            out.push(abs);
        }
    }
    out
}

fn is_denied(path: &Path, denies: &[PathBuf]) -> bool {
    denies.iter().any(|d| path.starts_with(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn denies_system_paths() {
        let cfg = CheckpointConfig::default();
        let out = keep_snapshottable(&[PathBuf::from("/etc/passwd")], &cfg);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn keeps_userland_files() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"hi").unwrap();
        let cfg = CheckpointConfig::default();
        let out = keep_snapshottable(std::slice::from_ref(&f), &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn keeps_missing_paths_for_creation() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("will-be-created.txt");
        let cfg = CheckpointConfig::default();
        let out = keep_snapshottable(std::slice::from_ref(&f), &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn skips_oversized_files() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("big.bin");
        std::fs::write(&f, vec![0u8; 1024 * 1024]).unwrap();
        let cfg = CheckpointConfig {
            max_file_bytes: 100,
            deny_prefixes: vec![],
        };
        let out = keep_snapshottable(std::slice::from_ref(&f), &cfg);
        assert!(out.is_empty());
    }

    #[test]
    fn dedups() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        let cfg = CheckpointConfig::default();
        let out = keep_snapshottable(&[f.clone(), f.clone()], &cfg);
        assert_eq!(out.len(), 1);
    }
}
