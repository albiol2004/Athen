//! macOS sandbox backend backed by `/usr/bin/sandbox-exec` (Seatbelt).
//!
//! Seatbelt ships with every macOS install since 10.5 — there's nothing for
//! the user to install. We generate a profile per [`SandboxProfile`], write
//! it to a tempfile, and run the user's command via `sandbox-exec -f <profile>
//! <cmd> <args...>`. The tempfile path is preferred over `-p '<profile>'`
//! because inline profiles need careful shell escaping when paths contain
//! spaces or quotes — file mode sidesteps it entirely.
//!
//! Profile shape mirrors the bwrap analogue on Linux: read everywhere, deny
//! writes by default, then re-allow writes inside each `allowed_paths` entry.
//! Network is left allowed under `RestrictedWrite` (matches bwrap's default
//! `shell_execute` flow); `NoNetwork` adds an explicit `(deny network*)`.
//!
//! Note: `sandbox-exec` is officially deprecated by Apple but is still
//! present and functional on every supported macOS version. The replacement
//! (entitlement-based App Sandbox) does not work for arbitrary child
//! processes the way Seatbelt does, which is why every desktop tool that
//! needs ad-hoc subprocess sandboxing on macOS still uses it.

#![cfg(target_os = "macos")]

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel, SandboxProfile};
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use tokio::process::Command;
use tracing::{debug, warn};

const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Returns true when the macOS `sandbox-exec` binary is present.
/// Used by `SandboxDetector` to populate `SandboxCapabilities::macos_sandbox`.
pub fn macos_capability() -> bool {
    Path::new(SANDBOX_EXEC_PATH).exists()
}

/// macOS sandbox executor.
pub struct MacOsSandbox;

impl MacOsSandbox {
    /// Generate a Seatbelt profile string for the given sandbox profile.
    ///
    /// Strategy mirrors bwrap on Linux:
    /// - `RestrictedWrite`: `(allow default)` then `(deny file-write*)` then
    ///   re-allow `(allow file-write* (subpath "..."))` per entry. Writes
    ///   to /tmp & co are NOT silently re-allowed — paths must be in
    ///   `allowed_paths` to be writable. Network is left at the default
    ///   (allowed) so things like `pip install` work.
    /// - `ReadOnly`: read everywhere, no writes anywhere. Network allowed.
    /// - `NoNetwork`: writes everywhere, no network.
    /// - `Full`: locked-down profile — read-only filesystem, no network.
    ///
    /// Paths in `allowed_paths` must be absolute. Relative paths would
    /// produce a non-loadable profile; we let `sandbox-exec` reject them
    /// rather than silently coerce, since it usually means the caller
    /// passed a workspace cwd that wasn't resolved.
    pub fn generate_seatbelt_profile(profile: &SandboxProfile) -> String {
        match profile {
            SandboxProfile::ReadOnly => "(version 1)\n\
                 (allow default)\n\
                 (deny file-write*)\n"
                .to_string(),
            SandboxProfile::RestrictedWrite { allowed_paths } => {
                let mut sb = String::from(
                    "(version 1)\n\
                     (allow default)\n\
                     (deny file-write*)\n",
                );
                for path in allowed_paths {
                    sb.push_str(&format!(
                        "(allow file-write* (subpath \"{}\"))\n",
                        escape_path(&path.display().to_string()),
                    ));
                }
                // Common writes that are practically required for most
                // commands to function (terminals, ttys, pseudo-fs).
                sb.push_str(
                    "(allow file-write-data (literal \"/dev/null\"))\n\
                     (allow file-write-data (literal \"/dev/dtracehelper\"))\n\
                     (allow file-write-data (regex #\"^/dev/tty.*\"))\n\
                     (allow file-write-data (regex #\"^/dev/fd/\\d+$\"))\n",
                );
                sb
            }
            SandboxProfile::NoNetwork => "(version 1)\n\
                 (allow default)\n\
                 (deny network*)\n"
                .to_string(),
            SandboxProfile::Full => "(version 1)\n\
                 (allow default)\n\
                 (deny file-write*)\n\
                 (deny network*)\n"
                .to_string(),
        }
    }
}

/// Escape characters that have special meaning inside Seatbelt string
/// literals. The grammar is Lisp-style — backslash and double-quote are
/// the only two characters we need to handle to keep `(subpath "...")`
/// well-formed when paths contain weird chars (spaces/parens are fine
/// inside quoted strings, but `"` and `\` aren't).
fn escape_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

#[async_trait]
impl SandboxExecutor for MacOsSandbox {
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities> {
        use crate::detect::SandboxDetector;
        Ok(SandboxDetector::detect().await)
    }

    async fn execute(
        &self,
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let profile = match sandbox {
            SandboxLevel::OsNative { profile } => profile,
            _ => {
                return Err(AthenError::Sandbox(
                    "MacOsSandbox requires SandboxLevel::OsNative".into(),
                ))
            }
        };

        let profile_text = Self::generate_seatbelt_profile(profile);

        // Write the profile to a tempfile so we don't need to escape it
        // through a shell. `tempfile::NamedTempFile` removes itself on
        // drop, which we hold across the await via the `_guard` binding.
        let mut profile_file = tempfile::Builder::new()
            .prefix("athen-sb-")
            .suffix(".sb")
            .tempfile()
            .map_err(|e| AthenError::Sandbox(format!("create seatbelt tempfile: {e}")))?;
        profile_file
            .write_all(profile_text.as_bytes())
            .map_err(|e| AthenError::Sandbox(format!("write seatbelt profile: {e}")))?;
        profile_file
            .flush()
            .map_err(|e| AthenError::Sandbox(format!("flush seatbelt profile: {e}")))?;
        let profile_path = profile_file.path().to_path_buf();

        debug!(
            command,
            profile = %profile_path.display(),
            "Executing sandbox-exec command"
        );

        let start = Instant::now();
        let mut cmd = Command::new(SANDBOX_EXEC_PATH);
        cmd.arg("-f").arg(&profile_path).arg(command).args(args);
        cmd.kill_on_drop(true);

        let output = cmd
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("sandbox-exec spawn failed: {e}")))?;
        let elapsed = start.elapsed();

        // Hold the tempfile alive until sandbox-exec has finished reading it.
        drop(profile_file);

        let result = SandboxOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            execution_time_ms: elapsed.as_millis() as u64,
        };

        if !output.status.success() {
            warn!(
                exit_code = result.exit_code,
                stderr = %result.stderr,
                "sandbox-exec command exited with non-zero status"
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn capability_matches_filesystem() {
        assert_eq!(macos_capability(), Path::new(SANDBOX_EXEC_PATH).exists());
    }

    #[test]
    fn restricted_write_profile_allows_only_listed_paths() {
        let profile = SandboxProfile::RestrictedWrite {
            allowed_paths: vec![
                PathBuf::from("/Users/alex/.athen"),
                PathBuf::from("/tmp/work"),
            ],
        };
        let text = MacOsSandbox::generate_seatbelt_profile(&profile);
        assert!(text.contains("(allow default)"));
        assert!(text.contains("(deny file-write*)"));
        assert!(text.contains("(allow file-write* (subpath \"/Users/alex/.athen\"))"));
        assert!(text.contains("(allow file-write* (subpath \"/tmp/work\"))"));
        // Network is left allowed for shell_execute (curl, pip, etc).
        assert!(!text.contains("(deny network"));
    }

    #[test]
    fn read_only_profile_blocks_all_writes() {
        let text = MacOsSandbox::generate_seatbelt_profile(&SandboxProfile::ReadOnly);
        assert!(text.contains("(deny file-write*)"));
        assert!(!text.contains("(allow file-write*"));
    }

    #[test]
    fn no_network_profile_blocks_network() {
        let text = MacOsSandbox::generate_seatbelt_profile(&SandboxProfile::NoNetwork);
        assert!(text.contains("(deny network*)"));
        assert!(!text.contains("(deny file-write*)"));
    }

    #[test]
    fn full_profile_locks_down_writes_and_network() {
        let text = MacOsSandbox::generate_seatbelt_profile(&SandboxProfile::Full);
        assert!(text.contains("(deny file-write*)"));
        assert!(text.contains("(deny network*)"));
    }

    #[test]
    fn escape_path_handles_quotes_and_backslashes() {
        assert_eq!(escape_path("/plain/path"), "/plain/path");
        assert_eq!(
            escape_path(r#"/weird/"quoted"/path"#),
            r#"/weird/\"quoted\"/path"#
        );
        assert_eq!(escape_path(r"/back\slash"), r"/back\\slash");
    }
}
