//! Ref naming + path normalisation.
//!
//! All branches live under `refs/heads/arc/<uuid>`, all action tags
//! under `refs/tags/action/<entry_id>`. Paths inside trees mirror the
//! absolute filesystem path minus the leading `/` (so
//! `/home/alex/main.rs` → `home/alex/main.rs`).

use std::path::{Component, Path, PathBuf};

pub fn arc_branch(arc_id: &str) -> String {
    format!("refs/heads/arc/{arc_id}")
}

pub fn action_tag(entry_id: &str) -> String {
    format!("refs/tags/action/{entry_id}")
}

/// Strip the filesystem root so the path can be used as a key in the
/// git tree. `/home/x/a.rs` → `home/x/a.rs`, `home/x/a.rs` → unchanged
/// (already relative).
pub fn strip_root(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                // `..` inside a snapshot path is a bug; treat defensively
                // by appending a literal so we don't escape upward.
                out.push("__parent__");
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// Inverse of `strip_root`: prepend `/` (on POSIX) to recover the
/// absolute path. On Windows the original drive letter is lost in
/// snapshot encoding — phase 1 targets Linux/macOS; Windows lands
/// later with its own component mapping.
pub fn abs_from_rel(rel: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    out.push(rel);
    out
}
