//! macOS sandbox backend stub.
//!
//! TODO: Implement using `sandbox-exec` with a Seatbelt profile of the form
//! `(version 1) (deny default) (allow file-read*) (allow file-write* (subpath "..."))`
//! generated per `SandboxProfile` (one `subpath` per writable path). The
//! `generate_seatbelt_profile` helper below is a starting point.

#![cfg(target_os = "macos")]

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel, SandboxProfile};
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};
use std::path::Path;

const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Returns true when the macOS `sandbox-exec` binary is present.
/// Used by `SandboxDetector` to populate `SandboxCapabilities::macos_sandbox`.
pub fn macos_capability() -> bool {
    Path::new(SANDBOX_EXEC_PATH).exists()
}

/// macOS sandbox executor.
///
/// Currently a stub: even when `sandbox-exec` is present we refuse to execute
/// rather than run unsandboxed, so the agent layer is forced to fall back to
/// container isolation or a refusal — same fallback shape the Linux backend
/// uses when bwrap is unavailable.
pub struct MacOsSandbox;

impl MacOsSandbox {
    /// Generate a Seatbelt profile string for the given sandbox profile.
    /// Kept as scaffolding for the eventual real implementation.
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

#[async_trait]
impl SandboxExecutor for MacOsSandbox {
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities> {
        use crate::detect::SandboxDetector;
        Ok(SandboxDetector::detect().await)
    }

    async fn execute(
        &self,
        _command: &str,
        _args: &[&str],
        _sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        Err(AthenError::Sandbox(
            "macOS sandbox-exec backend is not yet implemented; refusing to run unsandboxed for safety".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_matches_filesystem() {
        assert_eq!(macos_capability(), Path::new(SANDBOX_EXEC_PATH).exists());
    }

    #[tokio::test]
    async fn execute_returns_unimplemented_error() {
        let result = MacOsSandbox
            .execute(
                "/bin/echo",
                &["hi"],
                &SandboxLevel::OsNative {
                    profile: SandboxProfile::ReadOnly,
                },
            )
            .await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not yet implemented"));
    }
}
