//! Read-only git recognition layer for the upcoming Code Mode feature.
//!
//! This module is a *trusted, Athen-internal, read-only observer* over a real
//! git repository. It shells out to the system `git` binary directly (NOT via
//! `athen-shell` / the nushell sandbox — that path is for agent-driven, risk-gated
//! commands; this is internal observation) and parses git's stable porcelain
//! output formats.
//!
//! Everything degrades gracefully: a missing `git` binary or a non-repository
//! root yields `GitRepoState { is_repo: false, .. }` rather than an error. The
//! public entry point [`read_git_state`] never panics.
//!
//! The parsing logic is factored into pure free functions
//! ([`parse_status_v2`], [`parse_worktrees`], [`parse_log`]) so they can be
//! unit-tested with string fixtures without spawning `git`.

use std::path::Path;

/// A snapshot of a git repository's observable state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GitRepoState {
    pub root: String,
    pub is_repo: bool,
    /// Current branch name; `None` when HEAD is detached.
    pub head_branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: Vec<DirtyFile>,
    pub worktrees: Vec<WorktreeInfo>,
    pub recent_commits: Vec<CommitInfo>,
}

/// A single changed (or untracked) path in the working tree.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirtyFile {
    pub path: String,
    /// Short status, e.g. `"M"`, `"A"`, `"D"`, `"??"`.
    pub status: String,
}

/// One git worktree (the main checkout or a linked worktree).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub is_main: bool,
    pub locked: bool,
}

/// A recent commit (from `git log`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitInfo {
    pub hash: String,
    pub subject: String,
    pub author: String,
    pub timestamp: String,
}

impl GitRepoState {
    /// An empty, "not a repo" state for the given root.
    fn empty(root: &Path) -> Self {
        Self {
            root: root.to_string_lossy().into_owned(),
            is_repo: false,
            head_branch: None,
            detached: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            dirty: Vec::new(),
            worktrees: Vec::new(),
            recent_commits: Vec::new(),
        }
    }
}

/// Parsed `# branch.*` header fields from `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchInfo {
    pub head_branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// Read-only git state for `root`. Never panics; degrades to `is_repo: false`
/// when git is missing or `root` is not a repository.
pub async fn read_git_state(root: &Path) -> GitRepoState {
    // Gate on whether this is a work tree at all. A failed spawn (no git binary),
    // a non-zero exit, or output other than "true" all mean "not a repo here".
    let inside = run_git(root, &["rev-parse", "--is-inside-work-tree"]).await;
    let is_repo = matches!(inside.as_deref().map(str::trim), Some("true"));
    if !is_repo {
        return GitRepoState::empty(root);
    }

    let mut state = GitRepoState::empty(root);
    state.is_repo = true;

    // Branch header + dirty files come from one --porcelain=v2 --branch call.
    if let Some(out) = run_git(root, &["status", "--porcelain=v2", "--branch"]).await {
        let (branch, dirty) = parse_status_v2(&out);
        state.head_branch = branch.head_branch;
        state.detached = branch.detached;
        state.upstream = branch.upstream;
        state.ahead = branch.ahead;
        state.behind = branch.behind;
        state.dirty = dirty;
    }

    if let Some(out) = run_git(root, &["worktree", "list", "--porcelain"]).await {
        state.worktrees = parse_worktrees(&out);
    }

    // Empty repo (no commits) makes `git log` exit non-zero → run_git returns
    // None → empty vec, which is exactly what we want.
    if let Some(out) = run_git(
        root,
        &[
            "log",
            "-n",
            "20",
            "--pretty=format:%H%x1f%s%x1f%an%x1f%aI",
        ],
    )
    .await
    {
        state.recent_commits = parse_log(&out);
    }

    state
}

/// Run `git -C <root> <args...>` and return trimmed stdout on success, or `None`
/// on spawn failure / non-zero exit.
async fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Suppress the console flash a GUI parent would otherwise inherit:
        // CREATE_NO_WINDOW = 0x0800_0000.
        cmd.creation_flags(0x0800_0000);
    }

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(?args, error = %e, "git invocation failed to spawn");
            return None;
        }
    };

    if !output.status.success() {
        tracing::debug!(?args, code = ?output.status.code(), "git invocation returned non-zero");
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

/// Parse `git status --porcelain=v2 --branch` output into branch metadata and
/// the list of dirty/untracked files.
///
/// Header lines start with `#` (`# branch.head`, `# branch.upstream`,
/// `# branch.ab`). Entry lines are `1`/`2` (changed/renamed tracked),
/// `u` (unmerged), `?` (untracked).
pub fn parse_status_v2(output: &str) -> (BranchInfo, Vec<DirtyFile>) {
    let mut branch = BranchInfo::default();
    let mut dirty = Vec::new();

    for line in output.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            // Header line, e.g. "branch.head main".
            let mut parts = rest.splitn(2, ' ');
            let key = parts.next().unwrap_or("");
            let val = parts.next().unwrap_or("").trim();
            match key {
                "branch.head" => {
                    if val == "(detached)" {
                        branch.detached = true;
                        branch.head_branch = None;
                    } else {
                        branch.head_branch = Some(val.to_string());
                    }
                }
                "branch.upstream" if !val.is_empty() => {
                    branch.upstream = Some(val.to_string());
                }
                "branch.ab" => {
                    // Format: "+A -B".
                    for tok in val.split_whitespace() {
                        if let Some(n) = tok.strip_prefix('+') {
                            branch.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = tok.strip_prefix('-') {
                            branch.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        // Entry line.
        let mut fields = line.split(' ');
        match fields.next() {
            Some("?") => {
                // "? <path>"
                let path = fields.collect::<Vec<_>>().join(" ");
                if !path.is_empty() {
                    dirty.push(DirtyFile {
                        path,
                        status: "??".to_string(),
                    });
                }
            }
            Some("1") => {
                // "1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>"
                if let Some(df) = parse_changed_entry(line, /*renamed=*/ false) {
                    dirty.push(df);
                }
            }
            Some("2") => {
                // "2 <XY> <sub> ... <X><score> <path>\t<origPath>"
                if let Some(df) = parse_changed_entry(line, /*renamed=*/ true) {
                    dirty.push(df);
                }
            }
            Some("u") => {
                // "u <XY> <sub> ... <path>" — unmerged.
                if let Some(df) = parse_unmerged_entry(line) {
                    dirty.push(df);
                }
            }
            _ => {}
        }
    }

    (branch, dirty)
}

/// Derive a short status string ("M", "A", "D", "MM", ...) from a porcelain-v2
/// XY field, stripping the `.` placeholders git uses for "unmodified".
fn condense_xy(xy: &str) -> String {
    let condensed: String = xy.chars().filter(|c| *c != '.').collect();
    if condensed.is_empty() {
        // Should not happen for a listed entry, but be defensive.
        "M".to_string()
    } else {
        condensed
    }
}

/// Parse a `1`/`2` (changed / renamed-copied) porcelain-v2 entry line.
/// For renamed (`2`) lines the path is `<path>\t<origPath>`; we keep `<path>`.
fn parse_changed_entry(line: &str, renamed: bool) -> Option<DirtyFile> {
    let fields: Vec<&str> = line.split(' ').collect();
    // 1: record(0) XY(1) sub(2) mH(3) mI(4) mW(5) hH(6) hI(7) path(8..)
    // 2: same + Xscore(8) path(9..)
    let xy = fields.get(1)?;
    let status = condense_xy(xy);
    let path_start = if renamed { 9 } else { 8 };
    let rest = fields.get(path_start..)?;
    let joined = rest.join(" ");
    // For renamed entries the new and old paths are tab-separated.
    let path = joined.split('\t').next().unwrap_or("").to_string();
    if path.is_empty() {
        return None;
    }
    Some(DirtyFile { path, status })
}

/// Parse a `u` (unmerged) porcelain-v2 entry line.
/// "u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>"
fn parse_unmerged_entry(line: &str) -> Option<DirtyFile> {
    let fields: Vec<&str> = line.split(' ').collect();
    let xy = fields.get(1)?;
    let status = condense_xy(xy);
    let rest = fields.get(10..)?;
    let path = rest.join(" ");
    if path.is_empty() {
        return None;
    }
    Some(DirtyFile { path, status })
}

/// Parse `git worktree list --porcelain` output. Records are separated by blank
/// lines; the first record is the main worktree.
pub fn parse_worktrees(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut first = true;

    // Records are blank-line separated.
    for record in output.split("\n\n") {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }

        let mut path: Option<String> = None;
        let mut head = String::new();
        let mut branch: Option<String> = None;
        let mut locked = false;

        for line in record.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(p.trim().to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = h.trim().to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                let b = b.trim();
                // git emits the full ref, e.g. "refs/heads/main".
                let name = b.strip_prefix("refs/heads/").unwrap_or(b);
                branch = Some(name.to_string());
            } else if line == "detached" || line == "bare" {
                branch = None;
            } else if line == "locked" || line.starts_with("locked ") {
                locked = true;
            }
        }

        if let Some(path) = path {
            worktrees.push(WorktreeInfo {
                path,
                branch,
                head,
                is_main: first,
                locked,
            });
            first = false;
        }
    }

    worktrees
}

/// Parse `git log -n N --pretty=format:%H%x1f%s%x1f%an%x1f%aI` output. Fields
/// within a commit are separated by the 0x1f unit separator; commits by newline.
pub fn parse_log(output: &str) -> Vec<CommitInfo> {
    let mut commits = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\u{1f}');
        let hash = fields.next().unwrap_or("").to_string();
        let subject = fields.next().unwrap_or("").to_string();
        let author = fields.next().unwrap_or("").to_string();
        let timestamp = fields.next().unwrap_or("").to_string();
        if hash.is_empty() {
            continue;
        }
        commits.push(CommitInfo {
            hash,
            subject,
            author,
            timestamp,
        });
    }
    commits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_v2_branch_dirty_and_untracked() {
        // main, upstream set, ahead 2 / behind 1, one modified + one added +
        // one untracked file.
        let fixture = "\
# branch.oid abc123
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 .M N... 100644 100644 100644 aaa bbb src/lib.rs
1 A. N... 000000 100644 100644 000 ccc src/new.rs
? notes.txt
";
        let (branch, dirty) = parse_status_v2(fixture);
        assert_eq!(branch.head_branch.as_deref(), Some("main"));
        assert!(!branch.detached);
        assert_eq!(branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!(branch.ahead, 2);
        assert_eq!(branch.behind, 1);

        assert_eq!(dirty.len(), 3);
        assert_eq!(dirty[0].path, "src/lib.rs");
        assert_eq!(dirty[0].status, "M");
        assert_eq!(dirty[1].path, "src/new.rs");
        assert_eq!(dirty[1].status, "A");
        assert_eq!(dirty[2].path, "notes.txt");
        assert_eq!(dirty[2].status, "??");
    }

    #[test]
    fn status_v2_detached_head() {
        let fixture = "\
# branch.oid deadbeef
# branch.head (detached)
";
        let (branch, dirty) = parse_status_v2(fixture);
        assert!(branch.detached);
        assert_eq!(branch.head_branch, None);
        assert_eq!(branch.upstream, None);
        assert_eq!(branch.ahead, 0);
        assert_eq!(branch.behind, 0);
        assert!(dirty.is_empty());
    }

    #[test]
    fn status_v2_clean_repo_has_no_dirty() {
        let fixture = "\
# branch.oid abc123
# branch.head develop
# branch.upstream origin/develop
# branch.ab +0 -0
";
        let (branch, dirty) = parse_status_v2(fixture);
        assert_eq!(branch.head_branch.as_deref(), Some("develop"));
        assert!(dirty.is_empty());
        assert_eq!(branch.ahead, 0);
        assert_eq!(branch.behind, 0);
    }

    #[test]
    fn status_v2_renamed_entry_keeps_new_path() {
        // "2" record: XY=R., then the rename score field, then "new\told".
        let fixture =
            "2 R. N... 100644 100644 100644 aaa bbb R100 src/new_name.rs\tsrc/old_name.rs\n";
        let (_branch, dirty) = parse_status_v2(fixture);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].path, "src/new_name.rs");
        assert_eq!(dirty[0].status, "R");
    }

    #[test]
    fn worktrees_main_plus_linked() {
        let fixture = "\
worktree /home/alex/pruebas/Athen
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/alex/pruebas/Athen-feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feat/code-mode
locked
";
        let wts = parse_worktrees(fixture);
        assert_eq!(wts.len(), 2);

        assert_eq!(wts[0].path, "/home/alex/pruebas/Athen");
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[0].head, "1111111111111111111111111111111111111111");
        assert!(wts[0].is_main);
        assert!(!wts[0].locked);

        assert_eq!(wts[1].path, "/home/alex/pruebas/Athen-feature");
        assert_eq!(wts[1].branch.as_deref(), Some("feat/code-mode"));
        assert!(!wts[1].is_main);
        assert!(wts[1].locked);
    }

    #[test]
    fn worktrees_detached_linked() {
        let fixture = "\
worktree /repo
HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/main

worktree /repo/detached
HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
detached
";
        let wts = parse_worktrees(fixture);
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[1].path, "/repo/detached");
        assert_eq!(wts[1].branch, None);
        assert!(!wts[1].is_main);
    }

    #[test]
    fn log_parses_unit_separated_fields() {
        let fixture = "\
abc123\u{1f}Fix the bug\u{1f}Alex\u{1f}2026-06-25T10:00:00+00:00
def456\u{1f}Add feature\u{1f}Jordan\u{1f}2026-06-24T09:30:00+00:00";
        let commits = parse_log(fixture);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "abc123");
        assert_eq!(commits[0].subject, "Fix the bug");
        assert_eq!(commits[0].author, "Alex");
        assert_eq!(commits[0].timestamp, "2026-06-25T10:00:00+00:00");
        assert_eq!(commits[1].hash, "def456");
        assert_eq!(commits[1].subject, "Add feature");
    }

    #[test]
    fn log_empty_repo_is_empty_vec() {
        let commits = parse_log("");
        assert!(commits.is_empty());
    }
}
