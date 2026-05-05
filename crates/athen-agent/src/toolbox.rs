//! Persistent agent toolbox: pip-installed Python packages and
//! npm-installed Node packages live under `~/.athen/toolbox/` so they
//! survive reboots, unlike the bwrap-sandboxed `/tmp` workaround the
//! agent used to fall back on.
//!
//! The `shell_execute` tool injects `PYTHONPATH` and prepends the node
//! `bin/` directory onto `PATH` before every command, so installed
//! packages Just Work without the model having to remember env vars.

use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use athen_core::error::{AthenError, Result};
use athen_core::paths;

/// Which language runtime an entry in the toolbox belongs to.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Python,
    Node,
}

impl Runtime {
    pub fn as_str(&self) -> &'static str {
        match self {
            Runtime::Python => "python",
            Runtime::Node => "node",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "python3" | "pip" | "pip3" | "py" => Some(Runtime::Python),
            "node" | "npm" | "nodejs" | "js" => Some(Runtime::Node),
            _ => None,
        }
    }
}

/// One installed package recorded in `manifest.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub runtime: Runtime,
    pub package: String,
    pub version_spec: Option<String>,
    pub installed_version: Option<String>,
    pub reason: String,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub runtime_version: Option<String>,
}

/// On-disk manifest tracking which toolbox packages exist.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolboxManifest {
    pub installs: Vec<InstalledPackage>,
}

impl ToolboxManifest {
    /// Replace any existing entry for `(runtime, package)` with `pkg`,
    /// or append it if no match exists. Keeps the manifest from growing
    /// duplicates when the agent reinstalls or upgrades.
    pub fn upsert(&mut self, pkg: InstalledPackage) {
        if let Some(slot) = self
            .installs
            .iter_mut()
            .find(|p| p.runtime == pkg.runtime && p.package == pkg.package)
        {
            *slot = pkg;
        } else {
            self.installs.push(pkg);
        }
    }
}

/// Detected runtime versions, populated once per process by
/// [`probe_runtimes`]. Missing binaries are `None`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeProbe {
    pub python: Option<String>,
    pub pip: Option<String>,
    pub node: Option<String>,
    pub npm: Option<String>,
}

static RUNTIME_PROBE: StdMutex<Option<RuntimeProbe>> = StdMutex::new(None);

/// Probe Python, pip, Node, and npm once and cache the result. Each
/// probe walks a small list of platform-aware binary aliases, taking
/// the first that responds with a version string. On Windows the
/// official Python installer typically only exposes `python` and `pip`
/// (no `python3`/`pip3`), and the npm wrapper is `npm.cmd`; without
/// the alias list the toolbox prompt would always say "missing" on a
/// freshly-installed Windows host even when the runtime is present.
///
/// Each individual probe times out after 5s so a hung interpreter
/// doesn't stall startup. Missing binaries are recorded as `None`.
///
/// Cached for the rest of the process lifetime; the wizard's runtime
/// installer calls [`invalidate_runtime_probe_cache`] after a fresh
/// install so the next probe picks up the newly portable interpreter.
pub async fn probe_runtimes() -> RuntimeProbe {
    if let Some(p) = RUNTIME_PROBE.lock().expect("probe lock").clone() {
        return p;
    }
    let (python, pip, node, npm) = tokio::join!(
        probe_first(python_aliases(), &["--version"]),
        probe_first(pip_aliases(), &["--version"]),
        probe_first(node_aliases(), &["--version"]),
        probe_first(npm_aliases(), &["--version"]),
    );
    let probe = RuntimeProbe {
        python: python.map(extract_version),
        pip: pip.map(extract_version),
        node: node.map(extract_version),
        npm: npm.map(extract_version),
    };
    *RUNTIME_PROBE.lock().expect("probe lock") = Some(probe.clone());
    probe
}

/// Drop the cached runtime probe. Next call to [`probe_runtimes`] will
/// re-spawn the version checks. Called by the wizard after installing a
/// portable runtime so the new binary shows up in the prompt slot
/// without restarting the app.
pub fn invalidate_runtime_probe_cache() {
    *RUNTIME_PROBE.lock().expect("probe lock") = None;
}

/// Order matters: the install / uninstall functions also iterate these
/// lists and pick the first binary that successfully spawns, so the
/// most-likely-correct name should be first per platform.
pub(crate) fn python_aliases() -> &'static [&'static str] {
    if cfg!(windows) {
        &["python", "py", "python3"]
    } else {
        &["python3", "python"]
    }
}

pub(crate) fn pip_aliases() -> &'static [&'static str] {
    if cfg!(windows) {
        &["pip", "pip3"]
    } else {
        &["pip3", "pip"]
    }
}

pub(crate) fn node_aliases() -> &'static [&'static str] {
    &["node"]
}

pub(crate) fn npm_aliases() -> &'static [&'static str] {
    if cfg!(windows) {
        &["npm.cmd", "npm"]
    } else {
        &["npm"]
    }
}

/// Probe each candidate in order and return the version output of the
/// first one that exits successfully. `None` if every candidate fails
/// to spawn or returns non-zero.
async fn probe_first(candidates: &[&str], args: &[&str]) -> Option<String> {
    for bin in candidates {
        let Some(resolved) = resolve_executable(bin).await else {
            continue;
        };
        if let Some(out) = probe_one(&resolved, args).await {
            return Some(out);
        }
    }
    None
}

/// Spawn each candidate binary in order with the same args; the first
/// one that successfully starts (i.e. is on PATH and is executable) is
/// awaited and its `Output` returned, regardless of exit status. The
/// caller decides what to do with a non-zero exit. Only fails when
/// EVERY candidate yields an `ENOENT`-style spawn error — that's the
/// signal "the runtime isn't installed" and the error message lists
/// every name we tried so the user can install whichever one they
/// prefer.
///
/// On Windows each candidate is first resolved through `where.exe`,
/// and any hits under `Microsoft\WindowsApps\` are filtered out — those
/// are the App Execution Alias stubs that open the Microsoft Store
/// instead of running a real binary, and spawning one would either pop
/// the Store at the user or hang waiting on a network check.
async fn spawn_first(
    candidates: &[&str],
    fixed_args: &[&str],
    extra_args: &[&std::ffi::OsStr],
) -> std::result::Result<std::process::Output, String> {
    let mut last_err: Option<String> = None;
    for bin in candidates {
        let Some(resolved) = resolve_executable(bin).await else {
            tracing::debug!(bin, "spawn_first: skipped (no real exe on PATH)");
            last_err = Some(format!(
                "{bin}: not on PATH (or only a Windows Store alias)"
            ));
            continue;
        };
        let mut cmd = Command::new(&resolved);
        for a in fixed_args {
            cmd.arg(a);
        }
        for a in extra_args {
            cmd.arg(a);
        }
        match cmd.output().await {
            Ok(out) => return Ok(out),
            Err(e) => {
                tracing::debug!(bin, resolved = %resolved.display(), error = %e, "spawn_first: candidate failed");
                last_err = Some(format!("{bin}: {e}"));
            }
        }
    }
    Err(format!(
        "none of [{names}] could be spawned (last error: {err})",
        names = candidates.join(", "),
        err = last_err.as_deref().unwrap_or("no candidates"),
    ))
}

async fn probe_one(bin: &Path, args: &[&str]) -> Option<String> {
    let fut = async {
        Command::new(bin)
            .args(args)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                if s.trim().is_empty() {
                    s = String::from_utf8_lossy(&o.stderr).into_owned();
                }
                s
            })
            .filter(|s| !is_windows_store_alias_output(s))
    };
    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!("probe_runtimes: {} timed out after 5s", bin.display());
            None
        }
    }
}

/// Belt-and-suspenders: even after PATH-level filtering, some Windows
/// configurations route the alias through cmd.exe and we'd see the
/// "Python was not found; run without arguments to install from the
/// Microsoft Store" message in stdout WITH exit code 0. Treat any
/// version output that mentions the Store stub as "not installed".
fn is_windows_store_alias_output(s: &str) -> bool {
    let s = s.trim();
    s.contains("Microsoft Store") || s.contains("was not found")
}

/// Resolve `bin` to an absolute path. On Unix this is a passthrough —
/// `Command::new("python3")` already walks PATH correctly. On Windows
/// we run `where.exe <bin>` and pick the first hit that ISN'T under
/// `%LOCALAPPDATA%\Microsoft\WindowsApps\`, which is where Windows
/// puts the App Execution Alias shims for `python.exe`/`python3.exe`
/// that redirect to the Microsoft Store on activation.
///
/// Returns `None` when the binary isn't on PATH at all, or when every
/// hit is one of those Store shims (semantically: "no real interpreter
/// installed").
async fn resolve_executable(bin: &str) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        // Already absolute (e.g. portable bin) — trust the caller.
        let p = std::path::Path::new(bin);
        if p.is_absolute() {
            return Some(p.to_path_buf());
        }
        let out = Command::new("where.exe").arg(bin).output().await.ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if is_windows_apps_alias_path(line) {
                tracing::debug!(
                    bin,
                    path = line,
                    "skipping Microsoft Store App Execution Alias"
                );
                continue;
            }
            return Some(std::path::PathBuf::from(line));
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(std::path::PathBuf::from(bin))
    }
}

#[cfg(windows)]
fn is_windows_apps_alias_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains(r"\microsoft\windowsapps\")
}

/// Extract the first version-looking token from a `--version` output line,
/// e.g. `Python 3.13.5` → `3.13.5`, `v22.0.1` → `22.0.1`. Falls back to a
/// trimmed copy of the whole string when no digit-containing token exists.
fn extract_version(raw: String) -> String {
    let line = raw.lines().next().unwrap_or(raw.as_str()).trim();
    for tok in line.split_whitespace() {
        let t = tok.trim_start_matches('v');
        if t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return t.to_string();
        }
    }
    line.to_string()
}

/// Best-effort: create the python and node toolbox subdirectories if
/// they don't already exist. Failures are logged but never fatal —
/// install_*_package will surface a real error on first write.
pub async fn ensure_toolbox_dirs() {
    for dir in [
        paths::athen_toolbox_python_dir(),
        paths::athen_toolbox_node_dir(),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "failed to create toolbox subdir"
            );
        }
    }
}

/// Read the manifest from disk, returning [`ToolboxManifest::default`]
/// when the file is missing or unparseable.
pub async fn load_manifest() -> ToolboxManifest {
    let Some(path) = paths::athen_toolbox_manifest_path() else {
        return ToolboxManifest::default();
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ToolboxManifest::default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to read toolbox manifest, returning empty"
            );
            return ToolboxManifest::default();
        }
    };
    match serde_json::from_slice::<ToolboxManifest>(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "toolbox manifest could not be parsed, returning empty"
            );
            ToolboxManifest::default()
        }
    }
}

/// Write the manifest atomically (`manifest.json.tmp` → rename).
pub async fn save_manifest(m: &ToolboxManifest) -> Result<()> {
    let Some(path) = paths::athen_toolbox_manifest_path() else {
        return Err(AthenError::Other(
            "toolbox manifest path unavailable (no home dir)".into(),
        ));
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            AthenError::Other(format!(
                "failed to create toolbox dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let body = serde_json::to_vec_pretty(m).map_err(AthenError::from)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, &body).await.map_err(|e| {
        AthenError::Other(format!(
            "failed to write toolbox manifest tmp {}: {e}",
            tmp.display()
        ))
    })?;
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AthenError::Other(format!(
            "failed to rename toolbox manifest into place: {e}"
        )));
    }
    Ok(())
}

/// Install a Python package into `~/.athen/toolbox/python/` via
/// `pip3 install --target=<dir> --upgrade <spec>`. On success, appends
/// (or replaces) the matching manifest entry and saves.
pub async fn install_python_package(spec: &str, reason: &str) -> Result<InstalledPackage> {
    let target = paths::athen_toolbox_python_dir()
        .ok_or_else(|| AthenError::Other("toolbox python dir unavailable".into()))?;
    tokio::fs::create_dir_all(&target).await.map_err(|e| {
        AthenError::Other(format!(
            "failed to create toolbox python dir {}: {e}",
            target.display()
        ))
    })?;

    tracing::info!(spec, target = %target.display(), "pip install --target");
    let pip_args = [
        "install",
        "--upgrade",
        "--no-input",
        "--disable-pip-version-check",
        "--target",
    ];
    let extra: &[&std::ffi::OsStr] = &[target.as_os_str(), spec.as_ref()];
    let output = spawn_first(pip_aliases(), &pip_args, extra)
        .await
        .map_err(|spawn_err| {
            AthenError::Other(format!(
                "{spawn_err}. Install Python (which provides pip) first; the toolbox \
                 looked for: {names}",
                spawn_err = spawn_err,
                names = pip_aliases().join(", ")
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let msg = if !stderr.trim().is_empty() {
            stderr
        } else {
            stdout
        };
        return Err(AthenError::Other(format!(
            "pip install '{spec}' failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            msg.trim()
        )));
    }

    let installed_version = parse_pip_installed_version(
        &String::from_utf8_lossy(&output.stdout),
        &normalize_package_name(extract_pkg_name(spec)),
    );
    let probe = probe_runtimes().await;

    let pkg = InstalledPackage {
        runtime: Runtime::Python,
        package: extract_pkg_name(spec).to_string(),
        version_spec: Some(spec.to_string()),
        installed_version,
        reason: reason.to_string(),
        installed_at: chrono::Utc::now(),
        runtime_version: probe.python.clone(),
    };

    let mut m = load_manifest().await;
    m.upsert(pkg.clone());
    save_manifest(&m).await?;
    Ok(pkg)
}

/// Install a Node package into `~/.athen/toolbox/node/` via
/// `npm install --prefix=<dir> <spec>`. On success, appends (or
/// replaces) the matching manifest entry and saves.
pub async fn install_node_package(spec: &str, reason: &str) -> Result<InstalledPackage> {
    let target = paths::athen_toolbox_node_dir()
        .ok_or_else(|| AthenError::Other("toolbox node dir unavailable".into()))?;
    tokio::fs::create_dir_all(&target).await.map_err(|e| {
        AthenError::Other(format!(
            "failed to create toolbox node dir {}: {e}",
            target.display()
        ))
    })?;

    tracing::info!(spec, prefix = %target.display(), "npm install --prefix");
    let mut prefix_arg = std::ffi::OsString::from("--prefix=");
    prefix_arg.push(&target);

    let npm_args = ["install"];
    let extra: &[&std::ffi::OsStr] = &[
        prefix_arg.as_os_str(),
        "--no-fund".as_ref(),
        "--no-audit".as_ref(),
        "--save".as_ref(),
        spec.as_ref(),
    ];
    let output = spawn_first(npm_aliases(), &npm_args, extra)
        .await
        .map_err(|spawn_err| {
            AthenError::Other(format!(
                "{spawn_err}. Install Node.js (which provides npm) first; the \
                 toolbox looked for: {names}",
                spawn_err = spawn_err,
                names = npm_aliases().join(", ")
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let msg = if !stderr.trim().is_empty() {
            stderr
        } else {
            stdout
        };
        return Err(AthenError::Other(format!(
            "npm install '{spec}' failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            msg.trim()
        )));
    }

    let pkg_name = extract_pkg_name(spec).to_string();
    let installed_version = parse_npm_installed_version(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        &pkg_name,
    )
    .or_else(|| read_npm_version_from_node_modules(&target, &pkg_name));

    let probe = probe_runtimes().await;
    let pkg = InstalledPackage {
        runtime: Runtime::Node,
        package: pkg_name,
        version_spec: Some(spec.to_string()),
        installed_version,
        reason: reason.to_string(),
        installed_at: chrono::Utc::now(),
        runtime_version: probe.node.clone(),
    };

    let mut m = load_manifest().await;
    m.upsert(pkg.clone());
    save_manifest(&m).await?;
    Ok(pkg)
}

/// Remove a Python package from `~/.athen/toolbox/python/`. `pip
/// uninstall` doesn't support `--target`, so we walk the dist-info
/// RECORD file to delete the exact files pip wrote, then drop the
/// dist-info dir and the canonical package dir as a fallback. The
/// manifest entry is removed regardless; missing-on-disk packages
/// still get dropped from the manifest.
pub async fn uninstall_python_package(name: &str) -> Result<InstalledPackage> {
    let target = paths::athen_toolbox_python_dir()
        .ok_or_else(|| AthenError::Other("toolbox python dir unavailable".into()))?;

    let mut m = load_manifest().await;
    let normalized = normalize_package_name(name);
    let removed = m
        .installs
        .iter()
        .position(|p| {
            p.runtime == Runtime::Python && normalize_package_name(&p.package) == normalized
        })
        .map(|idx| m.installs.remove(idx))
        .ok_or_else(|| AthenError::Other(format!("'{name}' is not in the python toolbox")))?;

    if target.is_dir() {
        if let Some(di) = find_dist_info(&target, &normalized).await {
            // RECORD is "path,hash,size" lines; first column is relative to
            // the install dir (toolbox/python/). We delete each file plus
            // empty parent dirs we may have created.
            let record = tokio::fs::read_to_string(di.join("RECORD"))
                .await
                .unwrap_or_default();
            for line in record.lines() {
                let rel = line.split(',').next().unwrap_or("").trim();
                if rel.is_empty() {
                    continue;
                }
                let full = target.join(rel);
                let _ = tokio::fs::remove_file(&full).await;
            }
            let _ = tokio::fs::remove_dir_all(&di).await;
            // Best-effort: prune empty top-level pkg dirs the RECORD pass left.
            for entry_name in [
                normalized.clone(),
                normalized.replace('-', "_"),
                removed.package.clone(),
                removed.package.replace('-', "_"),
            ] {
                let candidate = target.join(&entry_name);
                if candidate.is_dir() && dir_is_empty(&candidate).await {
                    let _ = tokio::fs::remove_dir_all(&candidate).await;
                }
            }
        } else {
            // No dist-info found — fall back to deleting common shapes
            // (top-level package dir + any matching dist-info we may have
            // missed because of unusual naming).
            for entry_name in [
                normalized.clone(),
                normalized.replace('-', "_"),
                removed.package.clone(),
                removed.package.replace('-', "_"),
            ] {
                let candidate = target.join(&entry_name);
                if candidate.is_dir() {
                    let _ = tokio::fs::remove_dir_all(&candidate).await;
                }
            }
        }
    }

    save_manifest(&m).await?;
    Ok(removed)
}

/// Remove a Node package from `~/.athen/toolbox/node/` via
/// `npm uninstall --prefix=<dir> <pkg>`. The manifest entry is dropped
/// even if npm reports the package wasn't there.
pub async fn uninstall_node_package(name: &str) -> Result<InstalledPackage> {
    let target = paths::athen_toolbox_node_dir()
        .ok_or_else(|| AthenError::Other("toolbox node dir unavailable".into()))?;

    let mut m = load_manifest().await;
    let removed = m
        .installs
        .iter()
        .position(|p| p.runtime == Runtime::Node && p.package == name)
        .map(|idx| m.installs.remove(idx))
        .ok_or_else(|| AthenError::Other(format!("'{name}' is not in the node toolbox")))?;

    if target.is_dir() {
        let mut prefix_arg = std::ffi::OsString::from("--prefix=");
        prefix_arg.push(&target);
        let npm_args = ["uninstall"];
        let extra: &[&std::ffi::OsStr] = &[
            prefix_arg.as_os_str(),
            "--no-fund".as_ref(),
            "--no-audit".as_ref(),
            name.as_ref(),
        ];
        match spawn_first(npm_aliases(), &npm_args, extra).await {
            Ok(output) if !output.status.success() => {
                tracing::warn!(
                    package = name,
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "npm uninstall reported non-zero; manifest entry removed regardless"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    package = name,
                    error = %e,
                    "could not invoke npm to uninstall; manifest entry removed regardless"
                );
            }
        }
    }

    save_manifest(&m).await?;
    Ok(removed)
}

async fn find_dist_info(target: &Path, normalized_pkg: &str) -> Option<std::path::PathBuf> {
    let mut rd = tokio::fs::read_dir(target).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(prefix) = name_str.strip_suffix(".dist-info") else {
            continue;
        };
        // `pkg-X.Y.Z.dist-info` — strip the version suffix.
        let pkg_part = match prefix.rfind('-') {
            Some(idx) => &prefix[..idx],
            None => prefix,
        };
        if normalize_package_name(pkg_part) == normalized_pkg {
            return Some(entry.path());
        }
    }
    None
}

async fn dir_is_empty(p: &Path) -> bool {
    let Ok(mut rd) = tokio::fs::read_dir(p).await else {
        return false;
    };
    matches!(rd.next_entry().await, Ok(None))
}

/// Wipe both toolbox subtrees and the manifest, then recreate the
/// (empty) directories so subsequent `pip install --target` calls don't
/// have to.
pub async fn clear_toolbox() -> Result<()> {
    if let Some(dir) = paths::athen_toolbox_python_dir() {
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
                AthenError::Other(format!("failed to remove {}: {e}", dir.display()))
            })?;
        }
    }
    if let Some(dir) = paths::athen_toolbox_node_dir() {
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
                AthenError::Other(format!("failed to remove {}: {e}", dir.display()))
            })?;
        }
    }
    if let Some(path) = paths::athen_toolbox_manifest_path() {
        if path.exists() {
            tokio::fs::remove_file(&path).await.map_err(|e| {
                AthenError::Other(format!("failed to remove manifest {}: {e}", path.display()))
            })?;
        }
    }
    ensure_toolbox_dirs().await;
    Ok(())
}

/// Pre-fetched view of the toolbox surface, suitable for embedding
/// into a system prompt. Constructed once per LLM turn so the
/// prompt-building path can stay synchronous.
#[derive(Clone, Debug, Default)]
pub struct ToolboxPromptInfo {
    pub probe: RuntimeProbe,
    pub manifest: ToolboxManifest,
}

impl ToolboxPromptInfo {
    /// Load the runtime probe + on-disk manifest. Cheap on subsequent
    /// calls — the runtime probe is process-cached.
    pub async fn load() -> Self {
        let probe = probe_runtimes().await;
        let manifest = load_manifest().await;
        Self { probe, manifest }
    }
}

/// Build a one-line summary of the manifest for the system prompt.
/// Empty when no packages are installed. Sorted (runtime asc, then
/// name asc) so the prompt is stable across calls.
pub fn manifest_summary(m: &ToolboxManifest) -> String {
    if m.installs.is_empty() {
        return String::new();
    }
    let mut entries: Vec<&InstalledPackage> = m.installs.iter().collect();
    entries.sort_by(|a, b| {
        a.runtime
            .as_str()
            .cmp(b.runtime.as_str())
            .then_with(|| a.package.cmp(&b.package))
    });
    let parts: Vec<String> = entries
        .iter()
        .map(|p| {
            let reason = p.reason.trim();
            if reason.is_empty() {
                format!("{} ({})", p.package, p.runtime.as_str())
            } else {
                format!("{} ({}, {})", p.package, p.runtime.as_str(), reason)
            }
        })
        .collect();
    parts.join(", ")
}

/// Best-effort: pull the package name out of a pip/npm install spec
/// (`fpdf2>=2.7` → `fpdf2`, `@scope/pkg@1.0` → `@scope/pkg`).
fn extract_pkg_name(spec: &str) -> &str {
    let s = spec.trim();
    if let Some(stripped) = s.strip_prefix('@') {
        if let Some(idx) = stripped.find('@') {
            return &s[..idx + 1];
        }
        return s;
    }
    let cut = s
        .find(['=', '>', '<', '~', '!', '@', ' '])
        .unwrap_or(s.len());
    &s[..cut]
}

/// Normalize per PEP 503: lowercase + collapse runs of `-_.` to `-`.
fn normalize_package_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !prev_sep && !out.is_empty() {
                out.push('-');
                prev_sep = true;
            }
        } else {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    out
}

/// Look for `Successfully installed <name>-<ver>` in pip output and
/// pluck the version for the requested package.
fn parse_pip_installed_version(stdout: &str, normalized_name: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("Successfully installed") {
            for tok in rest.split_whitespace() {
                if let Some((n, v)) = tok.rsplit_once('-') {
                    if normalize_package_name(n) == normalized_name {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Try to extract `<pkg>@<ver>` from npm's "added 1 package" textual
/// output. npm 7+ usually only prints summaries — falling back to
/// reading `node_modules/<pkg>/package.json` covers the rest.
fn parse_npm_installed_version(stdout: &str, stderr: &str, pkg_name: &str) -> Option<String> {
    let needle = format!("{pkg_name}@");
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim_start_matches(['+', ' ']).trim();
        if let Some(idx) = line.find(&needle) {
            let after = &line[idx + needle.len()..];
            let v: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
                .collect();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn read_npm_version_from_node_modules(prefix: &Path, pkg_name: &str) -> Option<String> {
    let pj = prefix
        .join("node_modules")
        .join(pkg_name)
        .join("package.json");
    let text = std::fs::read_to_string(pj).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("version")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip() {
        let mut m = ToolboxManifest::default();
        m.upsert(InstalledPackage {
            runtime: Runtime::Python,
            package: "fpdf2".into(),
            version_spec: Some("fpdf2>=2.7".into()),
            installed_version: Some("2.8.1".into()),
            reason: "PDF generation".into(),
            installed_at: chrono::Utc::now(),
            runtime_version: Some("3.13.5".into()),
        });
        m.upsert(InstalledPackage {
            runtime: Runtime::Node,
            package: "playwright".into(),
            version_spec: None,
            installed_version: Some("1.45.0".into()),
            reason: "browser automation".into(),
            installed_at: chrono::Utc::now(),
            runtime_version: Some("22.0.1".into()),
        });

        let s = serde_json::to_string(&m).unwrap();
        let back: ToolboxManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.installs.len(), 2);
        assert_eq!(back.installs[0].runtime, Runtime::Python);
        assert_eq!(back.installs[0].package, "fpdf2");
        assert_eq!(back.installs[1].runtime, Runtime::Node);
        assert_eq!(
            back.installs[1].installed_version.as_deref(),
            Some("1.45.0")
        );
    }

    #[test]
    fn upsert_replaces_in_place() {
        let mut m = ToolboxManifest::default();
        m.upsert(InstalledPackage {
            runtime: Runtime::Python,
            package: "fpdf2".into(),
            version_spec: Some("fpdf2".into()),
            installed_version: Some("2.7.0".into()),
            reason: "v1".into(),
            installed_at: chrono::Utc::now(),
            runtime_version: None,
        });
        m.upsert(InstalledPackage {
            runtime: Runtime::Python,
            package: "fpdf2".into(),
            version_spec: Some("fpdf2>=2.8".into()),
            installed_version: Some("2.8.1".into()),
            reason: "v2".into(),
            installed_at: chrono::Utc::now(),
            runtime_version: None,
        });
        assert_eq!(m.installs.len(), 1);
        assert_eq!(m.installs[0].installed_version.as_deref(), Some("2.8.1"));
        assert_eq!(m.installs[0].reason, "v2");
    }

    #[test]
    fn manifest_summary_empty() {
        let m = ToolboxManifest::default();
        assert_eq!(manifest_summary(&m), "");
    }

    #[test]
    fn manifest_summary_populated_sorted() {
        let now = chrono::Utc::now();
        let m = ToolboxManifest {
            installs: vec![
                InstalledPackage {
                    runtime: Runtime::Python,
                    package: "requests".into(),
                    version_spec: None,
                    installed_version: None,
                    reason: "HTTP client".into(),
                    installed_at: now,
                    runtime_version: None,
                },
                InstalledPackage {
                    runtime: Runtime::Node,
                    package: "playwright".into(),
                    version_spec: None,
                    installed_version: None,
                    reason: "browser automation".into(),
                    installed_at: now,
                    runtime_version: None,
                },
                InstalledPackage {
                    runtime: Runtime::Python,
                    package: "fpdf2".into(),
                    version_spec: None,
                    installed_version: None,
                    reason: "PDF generation".into(),
                    installed_at: now,
                    runtime_version: None,
                },
            ],
        };
        let s = manifest_summary(&m);
        // node sorts before python (alphabetical on as_str) and within
        // each runtime, names are alphabetical.
        let expected = "playwright (node, browser automation), \
                        fpdf2 (python, PDF generation), \
                        requests (python, HTTP client)";
        assert_eq!(s, expected);
    }

    #[test]
    fn extract_pkg_name_handles_specs() {
        assert_eq!(extract_pkg_name("fpdf2"), "fpdf2");
        assert_eq!(extract_pkg_name("fpdf2>=2.7"), "fpdf2");
        assert_eq!(extract_pkg_name("requests==2.31.0"), "requests");
        assert_eq!(extract_pkg_name("requests~=2.31"), "requests");
        assert_eq!(extract_pkg_name("@scope/foo@1.0.0"), "@scope/foo");
        assert_eq!(extract_pkg_name("playwright@1.45"), "playwright");
    }

    #[test]
    fn normalize_package_name_pep503() {
        assert_eq!(normalize_package_name("Foo"), "foo");
        assert_eq!(normalize_package_name("Foo_Bar.baz"), "foo-bar-baz");
        assert_eq!(normalize_package_name("foo--__bar"), "foo-bar");
    }

    #[test]
    fn parse_pip_installed_version_finds_match() {
        let out = "\
Collecting fpdf2
  Downloading fpdf2-2.8.1-py2.py3-none-any.whl
Successfully installed defusedxml-0.7.1 fpdf2-2.8.1 Pillow-10.4.0
";
        assert_eq!(
            parse_pip_installed_version(out, "fpdf2").as_deref(),
            Some("2.8.1")
        );
        assert_eq!(
            parse_pip_installed_version(out, "pillow").as_deref(),
            Some("10.4.0")
        );
        assert_eq!(parse_pip_installed_version(out, "missing"), None);
    }

    #[test]
    fn extract_version_strips_prefixes() {
        assert_eq!(extract_version("Python 3.13.5".into()), "3.13.5");
        assert_eq!(extract_version("v22.0.1".into()), "22.0.1");
        assert_eq!(extract_version("npm 10.7.0".into()), "10.7.0");
    }

    #[tokio::test]
    async fn probe_runtimes_handles_missing_binary() {
        // We can't guarantee anything is installed in CI, so we just
        // require the call to return without panicking and to produce
        // a struct (Some/None per binary, both fine).
        let p = probe_runtimes().await;
        // Same call should hit the cache and be cheap.
        let p2 = probe_runtimes().await;
        assert_eq!(p.python, p2.python);
        assert_eq!(p.node, p2.node);
    }

    #[test]
    fn windows_store_alias_output_is_rejected() {
        // Real Python output should pass:
        assert!(!is_windows_store_alias_output("Python 3.12.7"));
        assert!(!is_windows_store_alias_output("v22.11.0"));
        // The Microsoft Store App Execution Alias message must be rejected
        // even when it arrives on stdout with a successful exit code:
        let alias = "Python was not found; run without arguments to install \
                     from the Microsoft Store, or disable this shortcut from \
                     Settings > Manage App Execution Aliases.";
        assert!(is_windows_store_alias_output(alias));
        assert!(is_windows_store_alias_output("Microsoft Store"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_apps_alias_path_detection() {
        assert!(is_windows_apps_alias_path(
            r"C:\Users\beta\AppData\Local\Microsoft\WindowsApps\python.exe"
        ));
        assert!(is_windows_apps_alias_path(
            r"C:\USERS\BETA\APPDATA\LOCAL\MICROSOFT\WINDOWSAPPS\PYTHON3.EXE"
        ));
        assert!(!is_windows_apps_alias_path(r"C:\Python312\python.exe"));
        assert!(!is_windows_apps_alias_path(
            r"C:\Users\beta\AppData\Local\Programs\Python\Python312\python.exe"
        ));
    }

    #[test]
    fn parse_npm_installed_version_from_summary() {
        let stdout = "+ playwright@1.45.0\nadded 12 packages in 3s\n";
        assert_eq!(
            parse_npm_installed_version(stdout, "", "playwright").as_deref(),
            Some("1.45.0")
        );
    }
}
