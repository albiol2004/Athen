//! macOS: sandbox-exec / App Sandbox.
//!
//! Uses the macOS `sandbox-exec` command with dynamically generated
//! Seatbelt profiles to restrict process capabilities.

#[cfg(target_os = "macos")]
use async_trait::async_trait;
#[cfg(target_os = "macos")]
use athen_core::error::{AthenError, Result};
#[cfg(target_os = "macos")]
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel, SandboxProfile};
#[cfg(target_os = "macos")]
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};
#[cfg(target_os = "macos")]
use std::time::Instant;
#[cfg(target_os = "macos")]
use tokio::process::Command;
#[cfg(target_os = "macos")]
use tracing::debug;

/// macOS sandbox executor using sandbox-exec with Seatbelt profiles.
#[cfg(target_os = "macos")]
pub struct MacOsSandbox;

#[cfg(target_os = "macos")]
impl MacOsSandbox {
    /// Generate a Seatbelt profile string for the given sandbox profile.
    pub fn generate_seatbelt_profile(profile: &SandboxProfile) -> String {
        match profile {
            SandboxProfile::ReadOnly => {
                "(version 1)\n(deny default)\n(allow file-read*)\n(allow process-exec)\n(allow process-fork)\n(allow sysctl-read)".to_string()
            }
            SandboxProfile::RestrictedWrite { allowed_paths } => {
                let mut sb = String::from("(version 1)\n(deny default)\n(allow file-read*)\n(allow process-exec)\n(allow process-fork)\n(allow sysctl-read)\n");
                for path in allowed_paths {
                    sb.push_str(&format!(
                        "(allow file-write* (subpath \"{}\"))\n",
                        path.display()
                    ));
                }
                sb
            }
            SandboxProfile::NoNetwork => {
                "(version 1)\n(deny default)\n(allow file-read*)\n(allow file-write*)\n(allow process-exec)\n(allow process-fork)\n(allow sysctl-read)\n(deny network*)".to_string()
            }
            SandboxProfile::Full => {
                "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n(allow sysctl-read)".to_string()
            }
        }
    }
}

#[cfg(target_os = "macos")]
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

        let seatbelt = Self::generate_seatbelt_profile(profile);
        debug!(%seatbelt, "Generated Seatbelt profile");

        let start = Instant::now();
        let output = Command::new("sandbox-exec")
            .arg("-p")
            .arg(&seatbelt)
            .arg(command)
            .args(args)
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("sandbox-exec failed: {e}")))?;

        let elapsed = start.elapsed();

        Ok(SandboxOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            execution_time_ms: elapsed.as_millis() as u64,
        })
    }
}
