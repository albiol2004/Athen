//! Process-based Nushell integration for cross-platform command execution.
//!
//! Executes commands via the `nu` binary if available on the system PATH,
//! falling back to the native platform shell otherwise.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use athen_core::error::{AthenError, Result};
use athen_core::traits::sandbox::SandboxOutput;
use athen_core::traits::shell::ShellExecutor;

use crate::native::NativeShell;

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Process-based Nushell executor.
///
/// If the `nu` binary is found on the system PATH, commands are executed
/// via `nu -c "command"`. Otherwise, all execution falls back to the
/// native platform shell.
pub struct NushellShell {
    /// Path to the `nu` binary, if found.
    nushell_path: Option<PathBuf>,
    /// Native shell fallback.
    native: NativeShell,
}

impl NushellShell {
    /// Create a new `NushellShell`, detecting whether `nu` is available.
    pub async fn new() -> Self {
        let native = NativeShell::new();
        let nushell_path = Self::detect_nushell(&native).await;

        if let Some(ref path) = nushell_path {
            info!(?path, "nushell detected");
        } else {
            info!("nushell not found, will use native shell fallback");
        }

        Self {
            nushell_path,
            native,
        }
    }

    /// Check if nushell is available on this system.
    pub fn is_available(&self) -> bool {
        self.nushell_path.is_some()
    }

    /// Return the path to the nushell binary, if found.
    pub fn nushell_path(&self) -> Option<&PathBuf> {
        self.nushell_path.as_ref()
    }

    /// Try to find the `nu` binary on PATH.
    async fn detect_nushell(native: &NativeShell) -> Option<PathBuf> {
        match native.which("nu").await {
            Ok(path) => path,
            Err(e) => {
                debug!("failed to detect nushell: {}", e);
                None
            }
        }
    }

    /// Execute a command via the nushell binary.
    async fn run_nushell(&self, command: &str) -> Result<SandboxOutput> {
        let nu_path = self
            .nushell_path
            .as_ref()
            .expect("run_nushell called without nushell available");

        debug!(command, "executing via nushell");

        let start = Instant::now();

        let mut cmd = Command::new(nu_path);
        cmd.arg("-c").arg(command);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            cmd.output(),
        )
        .await;

        let output = match result {
            Ok(inner) => inner?,
            Err(_) => {
                warn!(
                    command,
                    "nushell command timed out after {}s", DEFAULT_TIMEOUT_SECS
                );
                return Err(AthenError::Timeout(std::time::Duration::from_secs(
                    DEFAULT_TIMEOUT_SECS,
                )));
            }
        };

        let execution_time_ms = start.elapsed().as_millis() as u64;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        debug!(exit_code, execution_time_ms, "nushell command completed");

        Ok(SandboxOutput {
            exit_code,
            stdout,
            stderr,
            execution_time_ms,
        })
    }
}

#[async_trait]
impl ShellExecutor for NushellShell {
    /// Execute a command, preferring nushell if available.
    async fn execute(&self, command: &str) -> Result<SandboxOutput> {
        if self.is_available() {
            self.run_nushell(command).await
        } else {
            debug!("nushell not available, falling back to native shell");
            self.native.execute(command).await
        }
    }

    /// Always use the native platform shell for platform-specific commands.
    async fn execute_native(&self, command: &str) -> Result<SandboxOutput> {
        self.native.execute_native(command).await
    }

    /// Check if a program exists. Prefers nushell's `which` if available.
    async fn which(&self, program: &str) -> Result<Option<PathBuf>> {
        if self.is_available() {
            // Use nushell's `which` command. The output format may vary
            // across versions, so we run `which <program>` and parse the
            // path from the first line of stdout.
            let output = self
                .run_nushell(&format!("which {} | get path.0", program))
                .await?;
            if output.exit_code == 0 {
                let path = output.stdout.trim().to_string();
                if path.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(PathBuf::from(path)))
                }
            } else {
                // Nushell's which may fail for unknown programs; that's fine.
                Ok(None)
            }
        } else {
            self.native.which(program).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_nushell_creation() {
        let shell = NushellShell::new().await;
        // We don't assert is_available() since nu may or may not be installed,
        // but creation itself should always succeed.
        let _ = shell.is_available();
    }

    #[tokio::test]
    async fn test_execute_native_always_works() {
        let shell = NushellShell::new().await;
        let output = shell.execute_native("echo fallback").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "fallback");
    }

    #[tokio::test]
    async fn test_execute_succeeds_regardless_of_nushell() {
        // Whether nushell is installed or not, execute should work
        // (via nushell or native fallback).
        let shell = NushellShell::new().await;
        let output = shell.execute("echo works").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.trim().contains("works"));
    }

    #[tokio::test]
    async fn test_which_fallback() {
        let shell = NushellShell::new().await;
        // Nonexistent program should return None regardless of nushell availability.
        let result = shell.which("nonexistent_program_xyz_12345").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_fallback_when_nushell_absent() {
        // Construct a NushellShell with nushell_path = None to simulate absence.
        let shell = NushellShell {
            nushell_path: None,
            native: NativeShell::new(),
        };
        assert!(!shell.is_available());

        let output = shell.execute("echo from_native").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "from_native");
    }
}
