//! Cloudflare quick-tunnel manager.
//!
//! Self-contained module (no `AppState` coupling) that detects, installs
//! on demand, and runs a `cloudflared tunnel --url …` quick-tunnel so the
//! desktop HTTP listener gets a shareable `*.trycloudflare.com` URL with
//! zero Cloudflare account and zero port-forwarding. See
//! [`docs/REMOTE_ACCESS.md`] §5 for the design.
//!
//! Policy mirrors the portable Python/Node runtimes in
//! `athen-agent/src/runtimes.rs`:
//!
//! - Detect on `PATH` first; else install on demand into
//!   `<athen_data_dir>/toolbox/bin/` — never bundle, never copy.
//! - Download the per-platform static binary from the pinned cloudflared
//!   GitHub release. Linux/Windows ship a raw binary/`.exe`; macOS ships a
//!   `.tgz` containing a single `cloudflared`.
//!
//! Unlike the runtimes, cloudflared does **not** publish a uniform,
//! per-asset SHA-256 sidecar across every platform, so there is no
//! checksum pin here. We trust the same TLS origin (`github.com`) as the
//! download — acceptable for a tunnel helper, and the same trust boundary
//! the runtimes' checksum tripwire ultimately relies on (a compromised
//! origin would serve a matching sidecar too).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use athen_core::error::{AthenError, Result};
use athen_core::paths;

// ─── Pinned version ──────────────────────────────────────────────────

/// Pinned cloudflared release tag. Bump deliberately; the quick-tunnel
/// protocol is stable across these so an old pin keeps working.
const CLOUDFLARED_VERSION: &str = "2024.10.1";

/// How long to wait for cloudflared to print its `*.trycloudflare.com`
/// URL on stderr before we give up and kill the child.
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(25);

// ─── Progress reporting ──────────────────────────────────────────────

/// Progress callback passed into [`ensure_cloudflared`] so the Settings
/// panel can show "Downloading… 12 / 40 MB". Mirrors
/// `runtimes::ProgressCb`. Bytes are cumulative; `total` may be `None` if
/// the server didn't report content-length.
pub type ProgressCb = Arc<dyn Fn(TunnelInstallProgress) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TunnelInstallProgress {
    Resolving,
    Downloading { downloaded: u64, total: Option<u64> },
    Extracting,
    Done,
}

// ─── Binary location / detection ─────────────────────────────────────

/// Platform binary name.
fn bin_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// The managed install location: `<data_dir>/toolbox/bin/cloudflared`.
fn managed_path() -> Option<PathBuf> {
    Some(
        paths::athen_data_dir()?
            .join("toolbox")
            .join("bin")
            .join(bin_name()),
    )
}

/// Probe for an existing cloudflared binary: the managed location first,
/// then a `which`-style PATH scan. Returns `None` if not found.
pub fn cloudflared_path() -> Option<PathBuf> {
    if let Some(managed) = managed_path() {
        if managed.is_file() {
            return Some(managed);
        }
    }

    // Manual `which`: scan each PATH entry for the binary. We intentionally
    // do NOT add a `which` crate dependency.
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ─── Install on demand ───────────────────────────────────────────────

/// Resolve the per-platform download asset name (the file under the
/// release tag) and whether it is a raw binary or a `.tgz` to extract.
fn download_asset() -> Result<(&'static str, AssetKind)> {
    let arch = std::env::consts::ARCH;

    // Exactly one cfg block is compiled per target, so the active block
    // is this fn's tail expression — no `return` needed (clippy-clean).
    #[cfg(target_os = "linux")]
    {
        match arch {
            "x86_64" => Ok(("cloudflared-linux-amd64", AssetKind::RawBinary)),
            "aarch64" => Ok(("cloudflared-linux-arm64", AssetKind::RawBinary)),
            other => Err(unsupported(other)),
        }
    }

    #[cfg(target_os = "macos")]
    {
        match arch {
            "x86_64" => Ok(("cloudflared-darwin-amd64.tgz", AssetKind::TarGz)),
            "aarch64" => Ok(("cloudflared-darwin-arm64.tgz", AssetKind::TarGz)),
            other => Err(unsupported(other)),
        }
    }

    #[cfg(target_os = "windows")]
    {
        match arch {
            "x86_64" => Ok(("cloudflared-windows-amd64.exe", AssetKind::RawBinary)),
            other => Err(unsupported(other)),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(unsupported(arch))
    }
}

#[derive(Clone, Copy)]
enum AssetKind {
    /// The downloaded bytes are the cloudflared executable directly.
    RawBinary,
    /// A gzip'd tar containing a single `cloudflared` entry (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    TarGz,
}

fn unsupported(arch: &str) -> AthenError {
    AthenError::Other(format!(
        "no cloudflared build pinned for {}/{arch}",
        std::env::consts::OS
    ))
}

/// Return the detected cloudflared path, else download the per-platform
/// static binary into `<data_dir>/toolbox/bin/` and return its path.
pub async fn ensure_cloudflared(progress: Option<ProgressCb>) -> Result<PathBuf> {
    if let Some(existing) = cloudflared_path() {
        return Ok(existing);
    }

    emit(&progress, TunnelInstallProgress::Resolving);

    let (asset, kind) = download_asset()?;
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/{asset}"
    );

    let dest = managed_path()
        .ok_or_else(|| AthenError::Other("cloudflared install dir unavailable".into()))?;
    let bin_dir = dest
        .parent()
        .ok_or_else(|| AthenError::Other("cloudflared install dir has no parent".into()))?
        .to_path_buf();

    tracing::info!(%url, dest = %dest.display(), "installing cloudflared");

    let bytes = download_with_progress(&url, &progress).await?;

    let exe_bytes = match kind {
        AssetKind::RawBinary => bytes,
        AssetKind::TarGz => {
            emit(&progress, TunnelInstallProgress::Extracting);
            extract_cloudflared_from_tgz(bytes).await?
        }
    };

    tokio::fs::create_dir_all(&bin_dir)
        .await
        .map_err(|e| AthenError::Other(format!("create {} failed: {e}", bin_dir.display())))?;
    tokio::fs::write(&dest, &exe_bytes)
        .await
        .map_err(|e| AthenError::Other(format!("write {} failed: {e}", dest.display())))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&dest, perms)
            .await
            .map_err(|e| AthenError::Other(format!("chmod {} failed: {e}", dest.display())))?;
    }

    emit(&progress, TunnelInstallProgress::Done);
    tracing::info!(dest = %dest.display(), "cloudflared installed");
    Ok(dest)
}

fn emit(progress: &Option<ProgressCb>, ev: TunnelInstallProgress) {
    if let Some(cb) = progress {
        cb(ev);
    }
}

async fn download_with_progress(url: &str, progress: &Option<ProgressCb>) -> Result<Vec<u8>> {
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
        emit(
            progress,
            TunnelInstallProgress::Downloading { downloaded, total },
        );
    }
    Ok(buf)
}

/// macOS only: gz-decode + untar the `.tgz` and pull out the single
/// `cloudflared` entry. Runs the synchronous decode on a blocking thread.
#[cfg(target_os = "macos")]
async fn extract_cloudflared_from_tgz(bytes: Vec<u8>) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        use std::io::{Cursor, Read};
        let dec = flate2::read::GzDecoder::new(Cursor::new(bytes));
        let mut ar = tar::Archive::new(dec);
        for entry in ar
            .entries()
            .map_err(|e| AthenError::Other(format!("tar open failed: {e}")))?
        {
            let mut entry = entry.map_err(|e| AthenError::Other(format!("tar entry: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| AthenError::Other(format!("tar entry path: {e}")))?
                .into_owned();
            let is_cloudflared = path
                .file_name()
                .map(|n| n == "cloudflared")
                .unwrap_or(false);
            if is_cloudflared {
                let mut out = Vec::new();
                entry
                    .read_to_end(&mut out)
                    .map_err(|e| AthenError::Other(format!("read tar entry failed: {e}")))?;
                return Ok(out);
            }
        }
        Err(AthenError::Other(
            "cloudflared entry not found in .tgz".into(),
        ))
    })
    .await
    .map_err(|e| AthenError::Other(format!("extract task panicked: {e}")))?
}

/// Non-macOS stub: the `.tgz` path is unreachable off macOS (the asset
/// resolver never returns `TarGz`), but the function must still exist so
/// the match in `ensure_cloudflared` compiles on every platform.
#[cfg(not(target_os = "macos"))]
async fn extract_cloudflared_from_tgz(_bytes: Vec<u8>) -> Result<Vec<u8>> {
    Err(AthenError::Other(
        "tgz extraction is only used on macOS".into(),
    ))
}

// ─── Running the tunnel ──────────────────────────────────────────────

/// A live cloudflared quick-tunnel. Holds the child process and the
/// resolved public URL. Dropping it best-effort kills the child so a
/// dropped handle never leaks the process.
pub struct TunnelHandle {
    child: tokio::process::Child,
    pub url: String,
}

impl TunnelHandle {
    /// The public `*.trycloudflare.com` URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Stop the tunnel, killing the cloudflared child and reaping it.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        // Best-effort: don't block in Drop; just signal the kill. The OS
        // reaps the zombie when the tokio reactor's child watcher runs, or
        // at process exit.
        let _ = self.child.start_kill();
    }
}

/// Spawn `cloudflared tunnel --url http://127.0.0.1:<port> --no-autoupdate`,
/// read its stderr/stdout for the `*.trycloudflare.com` URL, and return a
/// handle to the still-running child. Times out after ~25s (killing the
/// child) if no URL appears.
pub async fn start_quick_tunnel(cloudflared: &Path, port: u16) -> Result<TunnelHandle> {
    let mut cmd = Command::new(cloudflared);
    cmd.args([
        "tunnel",
        "--url",
        &format!("http://127.0.0.1:{port}"),
        "--no-autoupdate",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    // GUI apps on Windows must not flash a console window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AthenError::Other(format!("spawn cloudflared failed: {e}")))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // cloudflared prints the URL to stderr; read stdout too for safety.
    let find_url = async {
        let mut out_lines = stdout.map(|s| BufReader::new(s).lines());
        let mut err_lines = stderr.map(|s| BufReader::new(s).lines());
        loop {
            let line: Option<String> = match (&mut err_lines, &mut out_lines) {
                (Some(e), Some(o)) => {
                    tokio::select! {
                        l = e.next_line() => l.ok().flatten(),
                        l = o.next_line() => l.ok().flatten(),
                    }
                }
                (Some(e), None) => e.next_line().await.ok().flatten(),
                (None, Some(o)) => o.next_line().await.ok().flatten(),
                (None, None) => None,
            };
            match line {
                Some(l) => {
                    if let Some(url) = parse_tunnel_url(&l) {
                        return Some(url);
                    }
                }
                None => return None, // both streams closed (process exited)
            }
        }
    };

    match tokio::time::timeout(TUNNEL_URL_TIMEOUT, find_url).await {
        Ok(Some(url)) => {
            tracing::info!(%url, "cloudflared quick-tunnel up");
            // Keep the child running; we no longer actively read its pipes.
            // cloudflared writes little after the URL line, so a full pipe
            // buffer is not a practical concern for a quick-tunnel.
            Ok(TunnelHandle { child, url })
        }
        Ok(None) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(AthenError::Other(
                "cloudflared exited before producing a tunnel URL".into(),
            ))
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(AthenError::Other(
                "cloudflared did not produce a tunnel URL in time".into(),
            ))
        }
    }
}

// ─── URL parsing (pure) ──────────────────────────────────────────────

const TRYCF_HOST: &str = ".trycloudflare.com";

/// Find and return the first `https://<sub>.trycloudflare.com` URL in a
/// line of cloudflared output. Pure; robust to surrounding ANSI / box
/// drawing characters cloudflared prints around the URL.
///
/// We implement this without `regex` (it is not a dependency of
/// `athen-app`) via a manual scan: locate `https://`, take the token up to
/// the next whitespace or box-drawing/control character, and accept it
/// only if the host part ends with `.trycloudflare.com`.
pub fn parse_tunnel_url(line: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find("https://") {
        let start = search_from + rel;
        // Take until the first character that can't be part of a URL.
        let rest = &line[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || is_url_boundary(c))
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if url_host_is_trycloudflare(candidate) {
            return Some(candidate.to_string());
        }
        // Advance past this `https://` to keep scanning.
        search_from = start + "https://".len();
    }
    None
}

/// Characters that terminate the URL token. cloudflared boxes the URL with
/// `|` and unicode box-drawing glyphs; treat the common fence characters
/// (and any non-ASCII, which covers box drawing) as boundaries.
fn is_url_boundary(c: char) -> bool {
    matches!(c, '|' | '"' | '\'' | '<' | '>' | '`' | '\\') || !c.is_ascii() || c.is_control()
}

/// True if `url` is `https://<host>` whose host ends with
/// `.trycloudflare.com`.
fn url_host_is_trycloudflare(url: &str) -> bool {
    let Some(after_scheme) = url.strip_prefix("https://") else {
        return false;
    };
    // Host is everything up to the first `/`, `?`, `#`, or `:`.
    let host = after_scheme
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or(after_scheme);
    // Must end with the suffix AND have a non-empty subdomain label before it.
    host.len() > TRYCF_HOST.len() && host.ends_with(TRYCF_HOST)
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_from_info_line() {
        let line =
            "2024-10-01T00:00:00Z INF |  https://exciting-test-words-here.trycloudflare.com  |";
        assert_eq!(
            parse_tunnel_url(line).as_deref(),
            Some("https://exciting-test-words-here.trycloudflare.com")
        );
    }

    #[test]
    fn parses_url_inside_box_border() {
        let line = "| https://abc-def-ghi.trycloudflare.com |";
        assert_eq!(
            parse_tunnel_url(line).as_deref(),
            Some("https://abc-def-ghi.trycloudflare.com")
        );
    }

    #[test]
    fn parses_url_with_unicode_box_drawing() {
        // cloudflared's real banner uses box-drawing glyphs around the URL.
        let line = "\u{2502}  https://random-subdomain-1234.trycloudflare.com  \u{2502}";
        assert_eq!(
            parse_tunnel_url(line).as_deref(),
            Some("https://random-subdomain-1234.trycloudflare.com")
        );
    }

    #[test]
    fn no_url_in_plain_log_line() {
        let line = "2024-10-01T00:00:00Z INF Requesting new quick Tunnel on trycloudflare.com...";
        assert_eq!(parse_tunnel_url(line), None);
    }

    #[test]
    fn ignores_non_trycloudflare_https_url() {
        let line = "INF connection registered to https://example.com/path?x=1";
        assert_eq!(parse_tunnel_url(line), None);
    }

    #[test]
    fn skips_non_matching_https_then_finds_match() {
        let line = "see https://example.com and https://my-tunnel-xyz.trycloudflare.com here";
        assert_eq!(
            parse_tunnel_url(line).as_deref(),
            Some("https://my-tunnel-xyz.trycloudflare.com")
        );
    }

    #[test]
    fn bare_suffix_without_subdomain_is_rejected() {
        // `https://.trycloudflare.com` (empty subdomain) must not match.
        let line = "weird https://.trycloudflare.com";
        assert_eq!(parse_tunnel_url(line), None);
    }

    #[test]
    fn bin_name_matches_platform() {
        if cfg!(target_os = "windows") {
            assert_eq!(bin_name(), "cloudflared.exe");
        } else {
            assert_eq!(bin_name(), "cloudflared");
        }
    }
}
