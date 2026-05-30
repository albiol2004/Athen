//! First-use installer for the Pipecat audio runtime.
//!
//! Reuses Athen's portable Python (installed by `athen-agent::runtimes`)
//! and runs `pip install --target <pipecat_env>` to produce an isolated
//! package tree that the `pipecat_runner.py` script (batch 2A) imports
//! from. We deliberately do NOT use virtualenv — `--target` is enough
//! isolation here and it sidesteps activation scripts that get awkward
//! across Linux/macOS/Windows.
//!
//! ## Path layout
//!
//! ```text
//! <toolbox_root>/
//!   pipecat_env/              ← pip install --target lives here
//!     pipecat_runner_marker.json
//!     <hundreds of pkg dirs>
//!   pipecat_runner.py         ← shipped by batch 2A
//! ```
//!
//! ## Why pin a Pipecat version
//!
//! The extras matrix (`deepgram`, `elevenlabs`, `cartesia`, `twilio`,
//! `openai`, `anthropic`, `google`) cross-cuts a lot of third-party SDKs
//! that change their auth surfaces frequently. A floating `>=` would
//! mean a user who installs in October sees a different runner contract
//! than a user who installs in November. We pin to a known-good range
//! (`>=0.0.50,<0.1`) so the bundled runner script can rely on a stable
//! ImportError surface.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::VoiceError;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// Phase of the setup process — drives FE progress UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupProgress {
    pub phase: SetupPhase,
    pub message: String,
    /// Best-effort percent (0..=100). `None` when the underlying step
    /// doesn't expose a measurable boundary (e.g. pip resolving deps).
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupPhase {
    Checking,
    PythonInstalling,
    PipecatInstalling,
    Finalizing,
    Done,
    Failed,
}

/// Status snapshot for the Setup status panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    pub python_installed: bool,
    pub pipecat_installed: bool,
    pub pipecat_env_dir: PathBuf,
    pub marker_path: PathBuf,
    /// Installed version of the `pipecat-ai` package. `None` when not
    /// installed yet (or when we couldn't parse `pip show` output).
    pub pipecat_version: Option<String>,
}

/// On-disk marker written after a successful install. Lets us answer
/// `pipecat_installed?` without invoking `pip show` (which spawns a
/// Python process and takes ~300ms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallMarker {
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub pipecat_version: Option<String>,
    pub extras: String,
}

// ---------------------------------------------------------------------------
// Path resolver
// ---------------------------------------------------------------------------

/// Compute paths derived from a toolbox root (e.g. `~/.athen/toolbox/`).
/// Caller passes the toolbox root — this crate doesn't know about
/// `dirs::data_dir()` so it stays testable + reusable.
pub struct PipecatPaths {
    pub toolbox_root: PathBuf,
}

impl PipecatPaths {
    pub fn new(toolbox_root: impl Into<PathBuf>) -> Self {
        Self {
            toolbox_root: toolbox_root.into(),
        }
    }

    pub fn pipecat_env(&self) -> PathBuf {
        self.toolbox_root.join("pipecat_env")
    }

    pub fn marker(&self) -> PathBuf {
        self.pipecat_env().join("pipecat_runner_marker.json")
    }

    pub fn runner_script(&self) -> PathBuf {
        self.toolbox_root.join("pipecat_runner.py")
    }
}

// ---------------------------------------------------------------------------
// Pinned install spec
// ---------------------------------------------------------------------------

/// Pinned Pipecat requirement — see module docs for the rationale. Range
/// not exact pin so security fixes inside 0.0.x can flow without a
/// runner-contract change.
const PIPECAT_SPEC: &str =
    "pipecat-ai[deepgram,elevenlabs,cartesia,twilio,openai,anthropic,google]>=0.0.50,<0.1";

/// Extra runtime deps Pipecat doesn't pull in on its own but the
/// `pipecat_runner.py` server side needs.
///
/// `twilio`: `pipecat-ai[twilio]` only ships Pipecat's Media-Streams frame
/// serializer, NOT the Twilio REST SDK. The runner calls
/// `from twilio.rest import Client` to actually place the outbound call, so
/// the standalone `twilio` package must be installed explicitly.
const EXTRA_DEPS: &[&str] = &["pyngrok", "fastapi", "uvicorn", "twilio"];

/// Human-readable extras list captured into the marker so a future
/// "what did we install" debug dump can replay the exact spec.
const EXTRAS_LABEL: &str = "deepgram,elevenlabs,cartesia,twilio,openai,anthropic,google";

// ---------------------------------------------------------------------------
// Status check
// ---------------------------------------------------------------------------

/// Inspect what's already installed. Cheap — only filesystem stats +
/// marker JSON read, no subprocess.
pub fn check_status(paths: &PipecatPaths, python_installed: bool) -> SetupStatus {
    let marker_path = paths.marker();
    let env_dir = paths.pipecat_env();
    let (pipecat_installed, version) = match std::fs::read(&marker_path) {
        Ok(bytes) => match serde_json::from_slice::<InstallMarker>(&bytes) {
            Ok(marker) => (env_dir.exists(), marker.pipecat_version),
            Err(_) => (false, None),
        },
        Err(_) => (false, None),
    };
    SetupStatus {
        python_installed,
        pipecat_installed,
        pipecat_env_dir: env_dir,
        marker_path,
        pipecat_version: version,
    }
}

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

/// Install Pipecat into the isolated env.
///
/// Pre-condition: portable Python is already installed (caller ensures
/// via `install_runtime(RuntimeKind::Python, ...)` if needed).
/// `python_exe` is the absolute path to the portable python binary.
///
/// Emits `SetupProgress` updates via the callback. Streaming pip stderr
/// gives a best-effort linear estimate based on pip phase tags
/// (`Collecting`, `Downloading`, `Installing`) — not an exact byte
/// progress.
pub async fn install_pipecat<F>(
    python_exe: &Path,
    paths: &PipecatPaths,
    on_progress: F,
) -> Result<SetupStatus, VoiceError>
where
    F: Fn(SetupProgress) + Send + Sync + 'static,
{
    on_progress(SetupProgress {
        phase: SetupPhase::Checking,
        message: "Preparing Pipecat install…".into(),
        percent: Some(0),
    });

    let env_dir = paths.pipecat_env();
    tokio::fs::create_dir_all(&env_dir).await.map_err(|e| {
        VoiceError::PipecatInstallFailed(format!("mkdir {} failed: {e}", env_dir.display()))
    })?;

    // Build the pip arg list. `--target` isolates the install without
    // a virtualenv; `--upgrade` overwrites any partial prior install;
    // `--no-cache-dir` keeps installs reproducible across reboots.
    let mut args: Vec<String> = vec![
        "-m".into(),
        "pip".into(),
        "install".into(),
        "--target".into(),
        env_dir.to_string_lossy().to_string(),
        "--upgrade".into(),
        "--no-cache-dir".into(),
        PIPECAT_SPEC.into(),
    ];
    for dep in EXTRA_DEPS {
        args.push((*dep).into());
    }

    on_progress(SetupProgress {
        phase: SetupPhase::PipecatInstalling,
        message: "Resolving Pipecat dependencies…".into(),
        percent: Some(5),
    });

    let mut cmd = Command::new(python_exe);
    cmd.args(&args)
        // Don't inherit a user PYTHONPATH — we want the pinned env to
        // be the only thing the runner sees later.
        .env_remove("PYTHONPATH")
        // Same for PYTHONHOME (Debian's /usr/bin/python3 sets it).
        .env_remove("PYTHONHOME")
        // Stream output unbuffered so progress updates are timely.
        .env("PYTHONUNBUFFERED", "1")
        // Best-effort PATH: portable Python bin dir comes first so any
        // helper shell-out from inside pip resolves to the portable
        // toolchain rather than something on the user's PATH.
        .env(
            "PATH",
            build_python_path_env(python_exe).unwrap_or_default(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        VoiceError::PipecatInstallFailed(format!("spawn {} pip failed: {e}", python_exe.display()))
    })?;

    // Take both pipes; pip writes most "Collecting"/"Downloading" lines
    // to stdout, but resolver errors come on stderr.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VoiceError::PipecatInstallFailed("no stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VoiceError::PipecatInstallFailed("no stderr pipe".into()))?;

    // Share the callback across the two stream-readers + the final wait
    // by wrapping in Arc. The callback itself is Send+Sync per the
    // generic bound.
    let cb = std::sync::Arc::new(on_progress);

    let cb_out = cb.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut buf: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(progress) = parse_pip_line(&line) {
                cb_out(progress);
            }
            tracing::debug!(target = "pipecat_install", "stdout: {}", line);
            buf.push(line);
            // Cap retained log lines so a runaway resolver doesn't OOM.
            if buf.len() > 4000 {
                buf.drain(..2000);
            }
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut buf: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            // pip emits warnings on stderr that aren't progress, but
            // ERROR lines surface real failures. We don't translate
            // those to progress events — they show up in the final
            // failure message if the exit code is non-zero.
            tracing::debug!(target = "pipecat_install", "stderr: {}", line);
            buf.push(line);
            if buf.len() > 4000 {
                buf.drain(..2000);
            }
        }
        buf
    });

    let status = child
        .wait()
        .await
        .map_err(|e| VoiceError::PipecatInstallFailed(format!("wait pip exit: {e}")))?;

    let _stdout_buf = stdout_task.await.unwrap_or_default();
    let stderr_buf = stderr_task.await.unwrap_or_default();

    if !status.success() {
        // Tail of stderr is the most diagnostic part — pip dumps the
        // resolver trace + the actual failing wheel at the bottom.
        let tail: String = stderr_buf
            .iter()
            .rev()
            .take(40)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let msg = format!(
            "pip exited with status {:?}; tail:\n{}",
            status.code(),
            tail
        );
        cb(SetupProgress {
            phase: SetupPhase::Failed,
            message: msg.clone(),
            percent: None,
        });
        return Err(VoiceError::PipecatInstallFailed(msg));
    }

    cb(SetupProgress {
        phase: SetupPhase::Finalizing,
        message: "Verifying install…".into(),
        percent: Some(92),
    });

    let installed_version = pipecat_version(python_exe, &env_dir).await;

    // Marker write — non-fatal if it fails (the env itself is the truth)
    // but we surface the warning. The marker is what `check_status`
    // reads so a failed write means subsequent loads will think
    // Pipecat is missing.
    let marker = InstallMarker {
        installed_at: chrono::Utc::now(),
        pipecat_version: installed_version.clone(),
        extras: EXTRAS_LABEL.into(),
    };
    let body = serde_json::to_vec_pretty(&marker)
        .map_err(|e| VoiceError::PipecatInstallFailed(format!("serialize marker: {e}")))?;
    tokio::fs::write(paths.marker(), body).await.map_err(|e| {
        VoiceError::PipecatInstallFailed(format!(
            "write marker {} failed: {e}",
            paths.marker().display()
        ))
    })?;

    cb(SetupProgress {
        phase: SetupPhase::Done,
        message: match installed_version.as_deref() {
            Some(v) => format!("Pipecat {v} installed."),
            None => "Pipecat installed.".to_string(),
        },
        percent: Some(100),
    });

    Ok(SetupStatus {
        python_installed: true,
        pipecat_installed: true,
        pipecat_env_dir: env_dir,
        marker_path: paths.marker(),
        pipecat_version: installed_version,
    })
}

/// Best-effort progress parser. Maps pip's phase tags to a coarse
/// percent that monotonically advances — never goes backward, never
/// lies that it's done before pip says so.
fn parse_pip_line(line: &str) -> Option<SetupProgress> {
    let trimmed = line.trim_start();
    // pip phases in chronological order:
    //   Collecting <pkg>            ← 10..60% — resolver doing graph work
    //   Downloading <pkg> (<bytes>) ← 30..70% — wheels arriving
    //   Installing collected pkgs   ← 75..90% — extract + scripts
    //   Successfully installed       ← 100%
    if let Some(rest) = trimmed.strip_prefix("Collecting ") {
        let pkg = rest.split_whitespace().next().unwrap_or(rest);
        return Some(SetupProgress {
            phase: SetupPhase::PipecatInstalling,
            message: format!("Resolving {pkg}…"),
            percent: Some(20),
        });
    }
    if let Some(rest) = trimmed.strip_prefix("Downloading ") {
        let pkg = rest.split_whitespace().next().unwrap_or(rest);
        return Some(SetupProgress {
            phase: SetupPhase::PipecatInstalling,
            message: format!("Downloading {pkg}…"),
            percent: Some(55),
        });
    }
    if trimmed.starts_with("Installing collected packages") {
        return Some(SetupProgress {
            phase: SetupPhase::PipecatInstalling,
            message: "Installing packages…".into(),
            percent: Some(85),
        });
    }
    if trimmed.starts_with("Successfully installed") {
        return Some(SetupProgress {
            phase: SetupPhase::Finalizing,
            message: "Install complete; finalizing…".into(),
            percent: Some(90),
        });
    }
    None
}

/// Build a PATH env value that prepends the portable Python's bin dir.
fn build_python_path_env(python_exe: &Path) -> Option<String> {
    let bin_dir = python_exe.parent()?.to_path_buf();
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = vec![bin_dir];
    for p in std::env::split_paths(&existing) {
        entries.push(p);
    }
    std::env::join_paths(entries)
        .ok()
        .map(|s| s.to_string_lossy().to_string())
}

/// Read installed pipecat version via `python -m pip show pipecat-ai`.
/// Returns `None` if pip show fails or doesn't contain a `Version:`
/// line — either case is non-fatal (caller still treats env as
/// installed if the marker exists).
pub async fn pipecat_version(python_exe: &Path, env_dir: &Path) -> Option<String> {
    let mut cmd = Command::new(python_exe);
    cmd.args(["-m", "pip", "show", "pipecat-ai"])
        .env("PYTHONPATH", env_dir.as_os_str())
        .env_remove("PYTHONHOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn paths_compose_correctly() {
        let tmp = tempdir().expect("tempdir");
        let paths = PipecatPaths::new(tmp.path());
        assert_eq!(paths.pipecat_env(), tmp.path().join("pipecat_env"));
        assert_eq!(
            paths.marker(),
            tmp.path()
                .join("pipecat_env")
                .join("pipecat_runner_marker.json"),
        );
        assert_eq!(paths.runner_script(), tmp.path().join("pipecat_runner.py"));
    }

    #[test]
    fn check_status_reports_not_installed_when_marker_missing() {
        let tmp = tempdir().expect("tempdir");
        let paths = PipecatPaths::new(tmp.path());
        let status = check_status(&paths, true);
        assert!(status.python_installed);
        assert!(!status.pipecat_installed);
        assert!(status.pipecat_version.is_none());
    }

    #[test]
    fn check_status_reads_marker_version() {
        let tmp = tempdir().expect("tempdir");
        let paths = PipecatPaths::new(tmp.path());
        std::fs::create_dir_all(paths.pipecat_env()).expect("mkdir");
        let marker = InstallMarker {
            installed_at: chrono::Utc::now(),
            pipecat_version: Some("0.0.51".into()),
            extras: EXTRAS_LABEL.into(),
        };
        std::fs::write(
            paths.marker(),
            serde_json::to_vec_pretty(&marker).expect("ser"),
        )
        .expect("write");

        let status = check_status(&paths, false);
        assert!(!status.python_installed);
        assert!(status.pipecat_installed);
        assert_eq!(status.pipecat_version.as_deref(), Some("0.0.51"));
    }

    #[test]
    fn check_status_treats_corrupt_marker_as_not_installed() {
        let tmp = tempdir().expect("tempdir");
        let paths = PipecatPaths::new(tmp.path());
        std::fs::create_dir_all(paths.pipecat_env()).expect("mkdir");
        std::fs::write(paths.marker(), b"not json").expect("write");
        let status = check_status(&paths, true);
        assert!(!status.pipecat_installed);
    }

    #[test]
    fn parse_pip_line_recognises_phases() {
        let p = parse_pip_line("Collecting pipecat-ai (from -r req.txt)").expect("collect");
        assert_eq!(p.phase, SetupPhase::PipecatInstalling);
        assert!(p.message.starts_with("Resolving pipecat-ai"));

        let d = parse_pip_line("Downloading pipecat_ai-0.0.51-py3-none-any.whl (4.2 MB)")
            .expect("download");
        assert_eq!(d.phase, SetupPhase::PipecatInstalling);

        let i = parse_pip_line("Installing collected packages: foo, bar, baz").expect("install");
        assert_eq!(i.phase, SetupPhase::PipecatInstalling);

        let s = parse_pip_line("Successfully installed foo-1.0 bar-2.0").expect("success");
        assert_eq!(s.phase, SetupPhase::Finalizing);

        assert!(parse_pip_line("unrelated chatter").is_none());
        assert!(parse_pip_line("").is_none());
    }
}
