//! Portable Python and Node runtimes installed on demand by the
//! onboarding wizard when the host doesn't already have them.
//!
//! Design rationale lives in `memory/project_runtime_auto_install.md`,
//! but the short version is:
//!
//! - Never bundle runtimes in the installer (50MB+ for users who
//!   already have Python).
//! - Never copy the user's existing install (breaks on Windows because
//!   of registry `PythonCore` keys, MSVC DLL linkage, scattered files).
//! - Detect at startup; install on demand into
//!   `<athen_data_dir>/toolbox/runtimes/{python,node}/`; prepend the
//!   resulting bin dirs to the process PATH so every other code path
//!   (probe, agent shell, `pip --target`, `npm --prefix`) keeps working
//!   unchanged.
//!
//! The pinned versions live in [`PYTHON_SPEC`] / [`NODE_SPEC`]. SHA-256
//! verification trusts the same TLS origin as the download (we fetch the
//! checksum sidecar published alongside the archive). This is a tripwire
//! against accidental corruption, not against a compromised origin.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::paths;

// ─── Pinned versions ─────────────────────────────────────────────────

const PYTHON_VERSION: &str = "3.12.7";
const PYTHON_RELEASE_TAG: &str = "20241016";

const NODE_VERSION: &str = "22.11.0";

// ─── Public types ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Python,
    Node,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Python => "python",
            RuntimeKind::Node => "node",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "py" => Some(RuntimeKind::Python),
            "node" | "nodejs" => Some(RuntimeKind::Node),
            _ => None,
        }
    }
}

/// Snapshot of a portable runtime install on disk. Persisted to the
/// runtime manifest after a successful install so the wizard can show
/// "installed v3.12.7" without re-probing the binary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortableRuntimeRecord {
    pub kind: RuntimeKind,
    pub version: String,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub source_url: String,
}

/// On-disk record of which portable runtimes the wizard has installed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimesManifest {
    pub installs: Vec<PortableRuntimeRecord>,
}

impl RuntimesManifest {
    pub fn upsert(&mut self, rec: PortableRuntimeRecord) {
        if let Some(slot) = self.installs.iter_mut().find(|r| r.kind == rec.kind) {
            *slot = rec;
        } else {
            self.installs.push(rec);
        }
    }

    pub fn get(&self, kind: RuntimeKind) -> Option<&PortableRuntimeRecord> {
        self.installs.iter().find(|r| r.kind == kind)
    }
}

/// Progress callback passed into the installers so the wizard can show
/// "Downloading… 12 / 60 MB" before the long extract step. Bytes are
/// cumulative; `total` may be `None` if the server didn't report
/// content-length.
pub type ProgressCb = Arc<dyn Fn(InstallProgress) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum InstallProgress {
    Resolving,
    Downloading { downloaded: u64, total: Option<u64> },
    Verifying,
    Extracting,
    Done,
}

// ─── Manifest of where to fetch each runtime ─────────────────────────

struct RuntimeArchiveSpec {
    /// Archive download URL.
    url: String,
    /// Sidecar URL whose body contains the expected hex SHA-256 of the
    /// archive (first whitespace-separated token used).
    sha256_url: String,
    /// `tar.gz` or `zip` — picks the extractor.
    format: ArchiveFormat,
    /// How many leading path components to strip during extraction so
    /// the contents land directly in `target_root` (the upstream
    /// archives put everything under a top-level versioned dir).
    strip_components: usize,
    /// Where the archive contents end up on disk.
    target_root: PathBuf,
    /// Human-readable version recorded in the manifest.
    version: String,
}

#[derive(Clone, Copy)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

fn python_spec() -> Result<RuntimeArchiveSpec> {
    let triple = python_triple()?;
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/\
         {tag}/cpython-{ver}+{tag}-{triple}-install_only.tar.gz",
        tag = PYTHON_RELEASE_TAG,
        ver = PYTHON_VERSION,
        triple = triple,
    );
    let sha256_url = format!("{url}.sha256");
    let target_root = paths::athen_portable_python_dir()
        .ok_or_else(|| AthenError::Other("portable python dir unavailable".into()))?;
    Ok(RuntimeArchiveSpec {
        url,
        sha256_url,
        format: ArchiveFormat::TarGz,
        // The python-build-standalone install_only archive expands to a
        // top-level `python/` directory.
        strip_components: 1,
        target_root,
        version: PYTHON_VERSION.to_string(),
    })
}

fn python_triple() -> Result<&'static str> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let t = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        other => {
            return Err(AthenError::Other(format!(
                "no portable Python build pinned for {os}/{arch} ({other:?})"
            )))
        }
    };
    Ok(t)
}

fn node_spec() -> Result<RuntimeArchiveSpec> {
    let (slug, format, strip) = node_slug()?;
    let url = format!(
        "https://nodejs.org/dist/v{ver}/node-v{ver}-{slug}.{ext}",
        ver = NODE_VERSION,
        slug = slug,
        ext = match format {
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::Zip => "zip",
        },
    );
    // Node publishes one SHASUMS256.txt for the whole release; the
    // installer parses out the matching line.
    let sha256_url = format!("https://nodejs.org/dist/v{NODE_VERSION}/SHASUMS256.txt");
    let target_root = paths::athen_portable_node_dir()
        .ok_or_else(|| AthenError::Other("portable node dir unavailable".into()))?;
    Ok(RuntimeArchiveSpec {
        url,
        sha256_url,
        format,
        strip_components: strip,
        target_root,
        version: NODE_VERSION.to_string(),
    })
}

fn node_slug() -> Result<(&'static str, ArchiveFormat, usize)> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    Ok(match (os, arch) {
        ("linux", "x86_64") => ("linux-x64", ArchiveFormat::TarGz, 1),
        ("linux", "aarch64") => ("linux-arm64", ArchiveFormat::TarGz, 1),
        ("macos", "x86_64") => ("darwin-x64", ArchiveFormat::TarGz, 1),
        ("macos", "aarch64") => ("darwin-arm64", ArchiveFormat::TarGz, 1),
        ("windows", "x86_64") => ("win-x64", ArchiveFormat::Zip, 1),
        ("windows", "aarch64") => ("win-arm64", ArchiveFormat::Zip, 1),
        _ => {
            return Err(AthenError::Other(format!(
                "no portable Node build pinned for {os}/{arch}"
            )))
        }
    })
}

// ─── Installed-state queries ─────────────────────────────────────────

pub fn is_portable_python_installed() -> bool {
    paths::athen_portable_python_bin()
        .map(|p| p.exists())
        .unwrap_or(false)
}

pub fn is_portable_node_installed() -> bool {
    paths::athen_portable_node_bin()
        .map(|p| p.exists())
        .unwrap_or(false)
}

// ─── Manifest IO ─────────────────────────────────────────────────────

fn manifest_path() -> Option<PathBuf> {
    paths::athen_runtimes_dir().map(|d| d.join("manifest.json"))
}

pub async fn load_manifest() -> RuntimesManifest {
    let Some(path) = manifest_path() else {
        return RuntimesManifest::default();
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RuntimesManifest::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "runtimes manifest read failed");
            return RuntimesManifest::default();
        }
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "runtimes manifest parse failed");
        RuntimesManifest::default()
    })
}

async fn save_manifest(m: &RuntimesManifest) -> Result<()> {
    let path = manifest_path()
        .ok_or_else(|| AthenError::Other("runtimes manifest path unavailable".into()))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AthenError::Other(format!("create {} failed: {e}", parent.display())))?;
    }
    let body = serde_json::to_vec_pretty(m).map_err(AthenError::from)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, &body)
        .await
        .map_err(|e| AthenError::Other(format!("write {} failed: {e}", tmp.display())))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| AthenError::Other(format!("rename runtimes manifest failed: {e}")))?;
    Ok(())
}

// ─── PATH bootstrapping ──────────────────────────────────────────────

/// Prepend portable runtime bin directories to the **process** PATH so
/// every subsequent `Command::new("python")` / `pip` / `node` / `npm`
/// call resolves to the portable copy. Idempotent — safe to call from
/// app startup AND after a fresh install.
///
/// Only adds dirs that exist on disk; missing dirs are ignored. Done at
/// process scope (not per-Command) so the existing `probe_runtimes()`
/// logic in `toolbox.rs` keeps working unchanged.
pub fn init_portable_path() {
    let mut new_entries: Vec<PathBuf> = Vec::new();
    for d in paths::athen_portable_python_path_dirs() {
        if d.exists() {
            new_entries.push(d);
        }
    }
    if let Some(d) = paths::athen_portable_node_bin_dir() {
        if d.exists() {
            new_entries.push(d);
        }
    }
    if new_entries.is_empty() {
        return;
    }

    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut existing_split: Vec<PathBuf> = std::env::split_paths(&existing).collect();

    // Skip entries we already prepended (idempotency on repeated calls).
    new_entries.retain(|d| !existing_split.iter().any(|e| e == d));
    if new_entries.is_empty() {
        return;
    }

    let mut combined = new_entries;
    combined.append(&mut existing_split);
    match std::env::join_paths(&combined) {
        Ok(joined) => {
            // SAFETY: `set_var` is unsafe in Rust 2024 but we're on edition
            // 2021. We do this at startup before spawning threads that read
            // env, and the prepended dirs are static program-lifetime data.
            std::env::set_var("PATH", joined);
            tracing::info!("prepended portable runtime dirs to PATH");
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not join portable runtime PATH entries");
        }
    }
}

// ─── Install pipeline ────────────────────────────────────────────────

/// Serialize concurrent install attempts of the same runtime so a fast
/// double-click in the wizard doesn't race two extracts into the same
/// dir. One mutex per process is enough — installs are rare.
static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

/// Download, verify, and extract the pinned portable runtime for `kind`.
/// Re-installing an already-present runtime is allowed (overwrites in
/// place) so the wizard's "Reinstall" path is the same as install.
pub async fn install_runtime(
    kind: RuntimeKind,
    progress: ProgressCb,
) -> Result<PortableRuntimeRecord> {
    let _guard = INSTALL_LOCK.lock().await;

    progress(InstallProgress::Resolving);
    let spec = match kind {
        RuntimeKind::Python => python_spec()?,
        RuntimeKind::Node => node_spec()?,
    };

    // Fresh dir each install — leftovers from a previous broken extract
    // would otherwise shadow the new layout.
    if spec.target_root.exists() {
        tokio::fs::remove_dir_all(&spec.target_root)
            .await
            .map_err(|e| {
                AthenError::Other(format!(
                    "could not clear existing {}: {e}",
                    spec.target_root.display()
                ))
            })?;
    }
    tokio::fs::create_dir_all(&spec.target_root)
        .await
        .map_err(|e| {
            AthenError::Other(format!(
                "could not create {}: {e}",
                spec.target_root.display()
            ))
        })?;

    let archive_filename = spec
        .url
        .rsplit('/')
        .next()
        .unwrap_or("runtime-archive")
        .to_string();

    let archive = download_with_progress(&spec.url, &progress).await?;

    progress(InstallProgress::Verifying);
    let expected_sha = fetch_expected_sha256(&spec.sha256_url, &archive_filename).await?;
    let actual_sha = hex_sha256(&archive);
    if !actual_sha.eq_ignore_ascii_case(&expected_sha) {
        return Err(AthenError::Other(format!(
            "SHA-256 mismatch for {archive_filename}: expected {expected_sha}, got {actual_sha}"
        )));
    }

    progress(InstallProgress::Extracting);
    let target_root = spec.target_root.clone();
    let format = spec.format;
    let strip = spec.strip_components;
    tokio::task::spawn_blocking(move || extract_archive(format, &archive, &target_root, strip))
        .await
        .map_err(|e| AthenError::Other(format!("extract task panicked: {e}")))??;

    // Make sure extracted unix binaries are executable. The tar crate
    // preserves mode for tarballs but zip does not — set +x defensively
    // on common bin paths.
    #[cfg(unix)]
    {
        ensure_unix_exec(&spec.target_root).await;
    }

    let record = PortableRuntimeRecord {
        kind,
        version: spec.version,
        installed_at: chrono::Utc::now(),
        source_url: spec.url,
    };
    let mut manifest = load_manifest().await;
    manifest.upsert(record.clone());
    save_manifest(&manifest).await?;

    init_portable_path();

    // The runtime probe in toolbox.rs caches its first result; let it
    // refresh on next call so the wizard sees the new install.
    super::toolbox::invalidate_runtime_probe_cache();

    progress(InstallProgress::Done);
    Ok(record)
}

async fn download_with_progress(url: &str, progress: &ProgressCb) -> Result<Vec<u8>> {
    use futures::StreamExt;

    let client = reqwest::Client::builder()
        .user_agent(concat!("Athen/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AthenError::Other(format!("http client init failed: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("GET {url} failed: {e}")))?
        .error_for_status()
        .map_err(|e| AthenError::Other(format!("GET {url} status: {e}")))?;
    let total = resp.content_length();

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AthenError::Other(format!("download chunk failed: {e}")))?;
        downloaded += chunk.len() as u64;
        buf.extend_from_slice(&chunk);
        progress(InstallProgress::Downloading { downloaded, total });
    }
    Ok(buf)
}

async fn fetch_expected_sha256(url: &str, archive_filename: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("Athen/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AthenError::Other(format!("http client init failed: {e}")))?;
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| AthenError::Other(format!("GET {url} failed: {e}")))?
        .error_for_status()
        .map_err(|e| AthenError::Other(format!("GET {url} status: {e}")))?
        .text()
        .await
        .map_err(|e| AthenError::Other(format!("read sha256 body failed: {e}")))?;

    // Two formats:
    //   1. Single-archive sidecar: "<hex>\n" or "<hex>  filename\n".
    //   2. SHASUMS256.txt: many lines "<hex>  <filename>".
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(first) = parts.next() else { continue };
        let rest = parts.next();
        match rest {
            None => {
                // Sidecar with just the digest.
                if is_hex_sha256(first) {
                    return Ok(first.to_string());
                }
            }
            Some(name) => {
                // SHASUMS256-style. Strip leading "*" (binary marker).
                let name = name.trim_start_matches('*');
                if name == archive_filename && is_hex_sha256(first) {
                    return Ok(first.to_string());
                }
            }
        }
    }
    Err(AthenError::Other(format!(
        "sha256 for {archive_filename} not found in {url}"
    )))
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn extract_archive(
    format: ArchiveFormat,
    bytes: &[u8],
    target_root: &Path,
    strip_components: usize,
) -> Result<()> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(bytes, target_root, strip_components),
        ArchiveFormat::Zip => extract_zip(bytes, target_root, strip_components),
    }
}

fn extract_tar_gz(bytes: &[u8], target_root: &Path, strip_components: usize) -> Result<()> {
    let dec = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut ar = tar::Archive::new(dec);
    ar.set_preserve_permissions(true);
    for entry in ar
        .entries()
        .map_err(|e| AthenError::Other(format!("tar open failed: {e}")))?
    {
        let mut entry = entry.map_err(|e| AthenError::Other(format!("tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| AthenError::Other(format!("tar entry path: {e}")))?
            .into_owned();
        let stripped = match strip_path(&path, strip_components) {
            Some(p) => p,
            None => continue,
        };
        let dest = target_root.join(stripped);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AthenError::Other(format!("mkdir {} failed: {e}", parent.display()))
            })?;
        }
        entry
            .unpack(&dest)
            .map_err(|e| AthenError::Other(format!("unpack {} failed: {e}", dest.display())))?;
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], target_root: &Path, strip_components: usize) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| AthenError::Other(format!("zip open failed: {e}")))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| AthenError::Other(format!("zip entry {i}: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let stripped = match strip_path(&rel, strip_components) {
            Some(p) => p,
            None => continue,
        };
        let dest = target_root.join(stripped);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest)
                .map_err(|e| AthenError::Other(format!("mkdir {} failed: {e}", dest.display())))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AthenError::Other(format!("mkdir {} failed: {e}", parent.display()))
            })?;
        }
        let mut out = std::fs::File::create(&dest)
            .map_err(|e| AthenError::Other(format!("create {} failed: {e}", dest.display())))?;
        let mut buf: Vec<u8> = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| AthenError::Other(format!("read zip entry failed: {e}")))?;
        std::io::Write::write_all(&mut out, &buf)
            .map_err(|e| AthenError::Other(format!("write {} failed: {e}", dest.display())))?;
    }
    Ok(())
}

fn strip_path(p: &Path, n: usize) -> Option<PathBuf> {
    if n == 0 {
        return Some(p.to_path_buf());
    }
    let mut comps = p.components();
    for _ in 0..n {
        comps.next()?;
    }
    let rest: PathBuf = comps.as_path().to_path_buf();
    if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest)
    }
}

#[cfg(unix)]
async fn ensure_unix_exec(target_root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["bin"] {
        let dir = target_root.join(sub);
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o111);
                let _ = tokio::fs::set_permissions(&path, perms).await;
            }
        }
    }
}

// ─── Wizard-facing snapshot ──────────────────────────────────────────

/// Bundle of "what does the host have RIGHT NOW" used to render the
/// runtimes step in the onboarding wizard.
#[derive(Clone, Debug, Serialize)]
pub struct RuntimesStatus {
    pub system_python: Option<String>,
    pub system_node: Option<String>,
    pub portable_python: Option<PortableRuntimeRecord>,
    pub portable_node: Option<PortableRuntimeRecord>,
    pub python_pinned_version: String,
    pub node_pinned_version: String,
    pub python_supported: bool,
    pub node_supported: bool,
}

pub async fn status() -> RuntimesStatus {
    let probe = super::toolbox::probe_runtimes().await;
    let manifest = load_manifest().await;
    RuntimesStatus {
        system_python: probe.python.clone(),
        system_node: probe.node.clone(),
        portable_python: manifest.get(RuntimeKind::Python).cloned(),
        portable_node: manifest.get(RuntimeKind::Node).cloned(),
        python_pinned_version: PYTHON_VERSION.to_string(),
        node_pinned_version: NODE_VERSION.to_string(),
        python_supported: python_triple().is_ok(),
        node_supported: node_slug().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_path_drops_leading_components() {
        let p = Path::new("python/bin/python3");
        let stripped = strip_path(p, 1).expect("stripped");
        assert_eq!(stripped, Path::new("bin/python3"));
    }

    #[test]
    fn strip_path_returns_none_when_only_top_level() {
        let p = Path::new("python");
        assert!(strip_path(p, 1).is_none());
    }

    #[test]
    fn hex_sha256_round_trip() {
        let h = hex_sha256(b"hello");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn is_hex_sha256_validates_length_and_chars() {
        assert!(is_hex_sha256(
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        ));
        assert!(!is_hex_sha256("not-a-hash"));
        assert!(!is_hex_sha256("2cf24dba"));
    }

    #[test]
    fn runtime_kind_round_trip() {
        assert_eq!(RuntimeKind::parse("python"), Some(RuntimeKind::Python));
        assert_eq!(RuntimeKind::parse("node"), Some(RuntimeKind::Node));
        assert_eq!(RuntimeKind::parse("nodejs"), Some(RuntimeKind::Node));
        assert_eq!(RuntimeKind::parse("rust"), None);
    }
}
