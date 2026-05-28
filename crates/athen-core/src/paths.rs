//! Platform-aware path helpers and a system-path read-only registry.
//!
//! These functions are pure path manipulation — they never read from or write
//! to the filesystem except `canonicalize_loose`, which only canonicalizes
//! when the path actually exists.

use std::path::{Component, Path, PathBuf};

/// User home directory.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Athen's per-user data directory.
///
/// - Unix: `~/.athen`
/// - Windows: `%APPDATA%\Athen`
pub fn athen_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir().map(|d| d.join("Athen"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        home_dir().map(|h| h.join(".athen"))
    }
}

/// Sandboxed file area under `athen_data_dir()`.
pub fn athen_files_sandbox() -> Option<PathBuf> {
    athen_data_dir().map(|d| d.join("files"))
}

/// Where sense crates persist downloaded attachments (email parts,
/// Telegram media). Each attachment lands under
/// `<root>/<event_id>/<index>_<sanitized_name>`. Lives under
/// [`athen_data_dir`] so a TTL purger can sweep it without walking the
/// whole data dir.
pub fn athen_attachments_dir() -> Option<PathBuf> {
    athen_data_dir().map(|d| d.join("sense-attachments"))
}

/// Default workspace directory the agent works inside when the user hasn't
/// pointed at a specific location. Relative paths in built-in file tools and
/// shell commands resolve against this dir, NOT the process cwd — so a fresh
/// `write { path: "test.html", ... }` lands here instead of the project the
/// app was launched from.
///
/// - `ATHEN_WORKSPACE_DIR` env var (if set & non-empty) takes precedence —
///   used by benchmark harnesses (TerminalBench, etc.) to point the agent
///   at the task's working directory instead of the user's data dir.
/// - Unix: `~/.athen/workspace`
/// - Windows: `%APPDATA%\Athen\workspace`
pub fn athen_workspace_dir() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("ATHEN_WORKSPACE_DIR") {
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    athen_data_dir().map(|d| d.join("workspace"))
}

/// Persistent toolbox root for shell-installed packages (pip --target,
/// npm --prefix). Lives under [`athen_data_dir`] so it survives reboots,
/// unlike `/tmp`.
pub fn athen_toolbox_dir() -> Option<PathBuf> {
    athen_data_dir().map(|d| d.join("toolbox"))
}

/// Subdirectory the agent's `pip3 install --target=...` writes into.
pub fn athen_toolbox_python_dir() -> Option<PathBuf> {
    athen_toolbox_dir().map(|d| d.join("python"))
}

/// Subdirectory the agent's `npm install --prefix=...` writes into.
/// Node's bin shims land in `<this>/bin`, the lib tree in `<this>/lib`.
pub fn athen_toolbox_node_dir() -> Option<PathBuf> {
    athen_toolbox_dir().map(|d| d.join("node"))
}

/// JSON manifest tracking what the agent has installed in the toolbox.
pub fn athen_toolbox_manifest_path() -> Option<PathBuf> {
    athen_toolbox_dir().map(|d| d.join("manifest.json"))
}

/// Root for portable language runtimes installed by the wizard when the
/// host doesn't already have Python / Node. Each subdir is one runtime.
pub fn athen_runtimes_dir() -> Option<PathBuf> {
    athen_toolbox_dir().map(|d| d.join("runtimes"))
}

/// Portable Python install root (python-build-standalone `install_only`
/// layout — `<root>/bin/python3` on Unix, `<root>/python.exe` on Windows).
pub fn athen_portable_python_dir() -> Option<PathBuf> {
    athen_runtimes_dir().map(|d| d.join("python"))
}

/// Where the portable Python interpreter actually lives. Used by the
/// install completion check and to prepend onto PATH.
pub fn athen_portable_python_bin() -> Option<PathBuf> {
    let root = athen_portable_python_dir()?;
    #[cfg(target_os = "windows")]
    {
        Some(root.join("python.exe"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(root.join("bin").join("python3"))
    }
}

/// Directories from the portable Python install that need to be on PATH
/// for `python` / `pip` / installed entry-points to resolve.
pub fn athen_portable_python_path_dirs() -> Vec<PathBuf> {
    let Some(root) = athen_portable_python_dir() else {
        return Vec::new();
    };
    #[cfg(target_os = "windows")]
    {
        // python.exe lives at the root; pip.exe + entry-point shims live
        // under Scripts/.
        vec![root.clone(), root.join("Scripts")]
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![root.join("bin")]
    }
}

/// Portable Node install root (nodejs.org dist tarball / zip layout).
pub fn athen_portable_node_dir() -> Option<PathBuf> {
    athen_runtimes_dir().map(|d| d.join("node"))
}

/// Where the portable `node` binary lives.
pub fn athen_portable_node_bin() -> Option<PathBuf> {
    let root = athen_portable_node_dir()?;
    #[cfg(target_os = "windows")]
    {
        Some(root.join("node.exe"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(root.join("bin").join("node"))
    }
}

/// Directory containing portable Node binaries (node, npm, npx).
pub fn athen_portable_node_bin_dir() -> Option<PathBuf> {
    let root = athen_portable_node_dir()?;
    #[cfg(target_os = "windows")]
    {
        Some(root)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(root.join("bin"))
    }
}

/// Resolve `p` against the agent workspace dir. Absolute paths pass through
/// unchanged; relative paths are joined with [`athen_workspace_dir`], or
/// with a `<temp>/athen-workspace` fallback when home isn't resolvable.
pub fn resolve_in_workspace(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let base =
        athen_workspace_dir().unwrap_or_else(|| std::env::temp_dir().join("athen-workspace"));
    base.join(p)
}

/// OS-specific list of read-only system roots.
pub fn system_readonly_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![
            PathBuf::from("/etc"),
            PathBuf::from("/usr"),
            PathBuf::from("/var"),
            PathBuf::from("/boot"),
            PathBuf::from("/sys"),
            PathBuf::from("/proc"),
            PathBuf::from("/dev"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/sbin"),
            PathBuf::from("/bin"),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/System"),
            PathBuf::from("/Library"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/private"),
            PathBuf::from("/var"),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![
            PathBuf::from(r"C:\Windows"),
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
            PathBuf::from(r"C:\ProgramData"),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// True if `p` is anywhere inside any of the OS system roots.
pub fn is_system_path(p: &Path) -> bool {
    for sys in system_readonly_paths() {
        if path_within(p, &sys) {
            return true;
        }
    }
    false
}

/// True if `child` is the same as or a descendant of `ancestor` after
/// best-effort canonicalization. On Windows the comparison is case-insensitive.
pub fn path_within(child: &Path, ancestor: &Path) -> bool {
    let c = canonicalize_loose(child);
    let a = canonicalize_loose(ancestor);

    #[cfg(target_os = "windows")]
    {
        let cs = c.to_string_lossy().to_lowercase();
        let as_ = a.to_string_lossy().to_lowercase();
        let cp = PathBuf::from(cs);
        let ap = PathBuf::from(as_);
        cp.starts_with(&ap)
    }
    #[cfg(not(target_os = "windows"))]
    {
        c.starts_with(&a)
    }
}

/// Canonicalize a path if it exists; otherwise normalize `.` and `..`
/// components manually without touching the filesystem. The original case is
/// preserved in the returned `PathBuf`.
///
/// On Windows the result is stripped of the `\\?\` verbatim prefix that
/// `std::fs::canonicalize` always emits. Without that, an existing file
/// canonicalizes to `\\?\C:\…` while a non-existent one normalizes to
/// `C:\…`, and any `path_within` / `starts_with` comparison between the
/// two yields a false negative — including the "inside Athen data dir →
/// Safe" gate, which would then prompt the user for permission to write
/// inside Athen's own workspace.
pub fn canonicalize_loose(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return strip_windows_verbatim(c);
    }
    normalize(p)
}

/// Strip the `\\?\` Win32 file-namespace prefix from an absolute Windows
/// path so it compares equal to the non-canonical form (`C:\…`). On
/// non-Windows targets this is a no-op.
#[inline]
fn strip_windows_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            // \\?\UNC\server\share\… → \\server\share\…
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Lexically resolve `.` and `..` against an absolute or relative path,
/// without touching the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut stack: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::ParentDir) | None => stack.push(Component::ParentDir),
                _ => {}
            },
            other => stack.push(other),
        }
    }
    let mut out = PathBuf::new();
    for c in stack {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Marker file/directory used to detect a project root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMarker {
    Git,
    Cargo,
    Npm,
    PyProject,
    GoMod,
    Maven,
    Gradle,
}

impl RootMarker {
    /// Human-readable label suitable for the approval button
    /// ("Allow ~/foo (git root)").
    pub fn label(self) -> &'static str {
        match self {
            RootMarker::Git => "git root",
            RootMarker::Cargo => "Cargo project",
            RootMarker::Npm => "npm project",
            RootMarker::PyProject => "Python project",
            RootMarker::GoMod => "Go module",
            RootMarker::Maven => "Maven project",
            RootMarker::Gradle => "Gradle project",
        }
    }
}

/// A detected project root with the marker that identified it.
#[derive(Debug, Clone)]
pub struct DetectedRoot {
    pub path: PathBuf,
    pub marker: RootMarker,
}

/// Walk parent directories from `start` looking for a project-root marker.
///
/// Priority order at each level: `.git/` directory, `Cargo.toml`,
/// `package.json`, `pyproject.toml`, `go.mod`, `pom.xml`,
/// `build.gradle`/`build.gradle.kts`.
///
/// Refuses to return a system path (see [`is_system_path`]) or the user's
/// home directory itself — a subdirectory of home is fine.
///
/// Returns `None` if we walk to the filesystem root without finding a
/// marker, or if every candidate is rejected by the safety checks.
/// Permission errors are treated as "marker not found, keep walking".
pub fn detect_project_root(start: &Path) -> Option<DetectedRoot> {
    if start.as_os_str().is_empty() {
        return None;
    }
    let canonical = canonicalize_loose(start);
    // If it's a file, start from parent. If it doesn't exist or we can't
    // tell, also try the parent (best effort).
    let mut current: PathBuf = match std::fs::metadata(&canonical) {
        Ok(md) if md.is_dir() => canonical.clone(),
        Ok(_) => canonical.parent()?.to_path_buf(),
        Err(_) => {
            // Non-existent or unreadable — try parent if any, else use as-is.
            canonical
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(canonical.clone())
        }
    };

    let home = home_dir().map(|h| canonicalize_loose(&h));

    loop {
        // Safety: refuse system paths and home itself.
        if is_system_path(&current) {
            return None;
        }
        if let Some(h) = home.as_ref() {
            if &current == h {
                return None;
            }
        }

        if let Some(marker) = detect_marker_at(&current) {
            return Some(DetectedRoot {
                path: current,
                marker,
            });
        }

        match current.parent() {
            Some(parent) if parent != current => {
                current = parent.to_path_buf();
            }
            _ => return None,
        }
    }
}

/// Check the priority-ordered list of marker files at `dir`.
/// Permission/IO errors are swallowed (treated as "not present").
fn detect_marker_at(dir: &Path) -> Option<RootMarker> {
    // `.git` may be a directory (normal repo) or a file (git submodule /
    // worktree). Either counts.
    if path_exists(&dir.join(".git")) {
        return Some(RootMarker::Git);
    }
    if path_exists(&dir.join("Cargo.toml")) {
        return Some(RootMarker::Cargo);
    }
    if path_exists(&dir.join("package.json")) {
        return Some(RootMarker::Npm);
    }
    if path_exists(&dir.join("pyproject.toml")) {
        return Some(RootMarker::PyProject);
    }
    if path_exists(&dir.join("go.mod")) {
        return Some(RootMarker::GoMod);
    }
    if path_exists(&dir.join("pom.xml")) {
        return Some(RootMarker::Maven);
    }
    if path_exists(&dir.join("build.gradle")) || path_exists(&dir.join("build.gradle.kts")) {
        return Some(RootMarker::Gradle);
    }
    None
}

/// `Path::exists` swallows permission errors as `false`; mirror that
/// shape explicitly so the intent is obvious at the call site.
#[inline]
fn path_exists(p: &Path) -> bool {
    std::fs::metadata(p).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_system_paths_detected() {
        assert!(is_system_path(Path::new("/etc/passwd")));
        assert!(is_system_path(Path::new("/usr/bin/ls")));
        assert!(is_system_path(Path::new("/proc/1/status")));
        assert!(is_system_path(Path::new("/etc")));
    }

    #[test]
    fn home_paths_not_system() {
        assert!(!is_system_path(Path::new("/home/user/Documents")));
        assert!(!is_system_path(Path::new("/tmp/foo")));
    }

    #[test]
    fn path_within_basic() {
        assert!(path_within(Path::new("/foo/bar/baz"), Path::new("/foo")));
        assert!(path_within(Path::new("/foo/bar"), Path::new("/foo/bar")));
        assert!(!path_within(Path::new("/foo/bar"), Path::new("/foo/baz")));
        assert!(!path_within(Path::new("/foo"), Path::new("/foo/bar")));
    }

    #[test]
    fn path_within_normalizes() {
        assert!(path_within(
            Path::new("/foo/bar/../bar/baz"),
            Path::new("/foo/bar"),
        ));
    }

    #[test]
    fn normalize_resolves_parent() {
        assert_eq!(normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("a/b/../../c")), PathBuf::from("c"));
        assert_eq!(normalize(Path::new("./.")), PathBuf::from("."));
    }

    #[test]
    fn athen_data_dir_returns_some() {
        assert!(athen_data_dir().is_some());
        let files = athen_files_sandbox().unwrap();
        assert!(files.ends_with("files"));
    }

    #[test]
    fn workspace_dir_under_data_dir() {
        let ws = athen_workspace_dir().expect("workspace");
        assert!(ws.ends_with("workspace"));
        let data = athen_data_dir().expect("data");
        assert!(ws.starts_with(&data));
    }

    #[test]
    fn toolbox_dirs_under_data_dir() {
        let tb = athen_toolbox_dir().expect("toolbox");
        assert!(tb.ends_with("toolbox"));
        let data = athen_data_dir().expect("data");
        assert!(tb.starts_with(&data));

        let py = athen_toolbox_python_dir().expect("python");
        assert!(py.ends_with("python"));
        assert!(py.starts_with(&tb));

        let node = athen_toolbox_node_dir().expect("node");
        assert!(node.ends_with("node"));
        assert!(node.starts_with(&tb));

        let m = athen_toolbox_manifest_path().expect("manifest");
        assert!(m.ends_with("manifest.json"));
        assert!(m.starts_with(&tb));
    }

    #[test]
    fn resolve_relative_uses_workspace() {
        let resolved = resolve_in_workspace(Path::new("test.html"));
        let ws = athen_workspace_dir().unwrap();
        assert_eq!(resolved, ws.join("test.html"));
    }

    #[test]
    fn resolve_absolute_unchanged() {
        let abs = Path::new("/tmp/x");
        assert_eq!(resolve_in_workspace(abs), PathBuf::from("/tmp/x"));
    }

    /// Regression: on Windows, `std::fs::canonicalize` emits paths with a
    /// `\\?\` verbatim prefix when the file exists, but the lexical
    /// `normalize` fallback used for non-existent paths does not. Without
    /// stripping the prefix, `path_within` returned false for files about
    /// to be created inside the Athen data dir → permission prompts on
    /// every write inside Athen's own workspace.
    #[test]
    #[cfg(target_os = "windows")]
    fn strip_verbatim_prefix_makes_canonical_and_lexical_match() {
        let canonical = PathBuf::from(r"\\?\C:\Users\Bob\AppData\Roaming\Athen");
        let lexical = PathBuf::from(r"C:\Users\Bob\AppData\Roaming\Athen\workspace\test.html");
        let stripped = strip_windows_verbatim(canonical);
        assert_eq!(
            stripped,
            PathBuf::from(r"C:\Users\Bob\AppData\Roaming\Athen")
        );
        // The lexical descendant must compare as inside the stripped ancestor.
        assert!(lexical.starts_with(&stripped));

        // UNC verbatim paths round-trip back to the non-verbatim UNC form.
        let unc = PathBuf::from(r"\\?\UNC\server\share\dir");
        assert_eq!(
            strip_windows_verbatim(unc),
            PathBuf::from(r"\\server\share\dir")
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn strip_verbatim_prefix_is_noop_on_unix() {
        let p = PathBuf::from("/home/alex/.athen");
        assert_eq!(strip_windows_verbatim(p.clone()), p);
    }

    // -------- detect_project_root --------

    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detect_root_git_precedence_over_cargo() {
        let td = tempdir().unwrap();
        let root = td.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]").unwrap();

        let got = detect_project_root(root).expect("should detect");
        assert_eq!(got.marker, RootMarker::Git);
        assert_eq!(canonicalize_loose(&got.path), canonicalize_loose(root));
    }

    #[test]
    fn detect_root_cargo_fallback_when_no_git() {
        let td = tempdir().unwrap();
        let root = td.path();
        fs::write(root.join("Cargo.toml"), "[package]").unwrap();

        let got = detect_project_root(root).expect("should detect");
        assert_eq!(got.marker, RootMarker::Cargo);
    }

    #[test]
    fn detect_root_package_json_fallback() {
        let td = tempdir().unwrap();
        let root = td.path();
        fs::write(root.join("package.json"), "{}").unwrap();

        let got = detect_project_root(root).expect("should detect");
        assert_eq!(got.marker, RootMarker::Npm);
    }

    #[test]
    fn detect_root_walks_up_from_nested_file() {
        let td = tempdir().unwrap();
        let root = td.path();
        fs::create_dir(root.join(".git")).unwrap();
        let deep = root.join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        let file = deep.join("file.rs");
        fs::write(&file, "// hi").unwrap();

        let got = detect_project_root(&file).expect("should walk up");
        assert_eq!(got.marker, RootMarker::Git);
        assert_eq!(canonicalize_loose(&got.path), canonicalize_loose(root));
    }

    #[test]
    fn detect_root_none_when_no_markers_walking_to_root() {
        let td = tempdir().unwrap();
        // Nested dir, no marker anywhere.
        let sub = td.path().join("x").join("y");
        fs::create_dir_all(&sub).unwrap();
        // No marker between `sub` and `/` (the tempdir is under
        // /tmp or similar — `/tmp` itself has no marker, and the walk
        // stops at `/` returning None).
        let got = detect_project_root(&sub);
        assert!(got.is_none(), "expected None, got {:?}", got);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detect_root_refuses_system_path() {
        // /etc itself is a system path even if a marker happened to exist.
        assert!(detect_project_root(Path::new("/etc")).is_none());
        assert!(detect_project_root(Path::new("/usr/local")).is_none());
    }

    #[test]
    fn detect_root_refuses_home_itself() {
        let Some(home) = home_dir() else { return };
        // Don't actually create files in $HOME; just verify that
        // even if we point AT $HOME, we get None (no marker), and
        // that asking about $HOME itself is safely rejected by the
        // home guard rather than promoted to a root.
        let got = detect_project_root(&home);
        // If the user happens to have a marker right at $HOME (rare),
        // we still refuse it explicitly.
        assert!(
            got.is_none(),
            "home dir itself must never be returned as a project root: {:?}",
            got
        );
    }

    #[test]
    fn detect_root_empty_path_returns_none() {
        assert!(detect_project_root(Path::new("")).is_none());
    }

    #[test]
    fn detect_root_nonexistent_path_returns_none_gracefully() {
        // A made-up path that almost certainly doesn't exist and has no
        // markers up the parent chain. Must not panic.
        let bogus = std::env::temp_dir()
            .join(format!("athen-nope-{}", uuid::Uuid::new_v4()))
            .join("deeper")
            .join("file.rs");
        // No panic; result is whatever the parent walk produces (likely
        // None, but if /tmp's ancestors had a marker, that's fine too —
        // the point is the call doesn't blow up).
        let _ = detect_project_root(&bogus);
    }

    #[test]
    fn root_marker_labels_are_distinct() {
        let labels = [
            RootMarker::Git.label(),
            RootMarker::Cargo.label(),
            RootMarker::Npm.label(),
            RootMarker::PyProject.label(),
            RootMarker::GoMod.label(),
            RootMarker::Maven.label(),
            RootMarker::Gradle.label(),
        ];
        let mut seen = std::collections::HashSet::new();
        for l in labels {
            assert!(seen.insert(l), "duplicate label: {}", l);
            assert!(!l.is_empty());
        }
    }
}
