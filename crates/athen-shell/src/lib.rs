//! Cross-platform shell execution for Athen.
//!
//! Primary: Nushell (process-based) for consistent cross-platform behavior.
//! Fallback: native platform shell (sh/bash on Unix, cmd on Windows) for
//! platform-specific tools.

pub mod native;
pub mod nushell;

use std::path::PathBuf;

use async_trait::async_trait;
use tracing::debug;

use athen_core::error::Result;
use athen_core::traits::sandbox::SandboxOutput;
use athen_core::traits::shell::ShellExecutor;

use crate::native::NativeShell;
use crate::nushell::NushellShell;

/// Unified shell facade that auto-detects nushell availability and provides
/// convenient methods for command execution.
///
/// - `execute()` prefers nushell, falls back to native shell.
/// - `execute_native()` always uses the native platform shell.
/// - `which()` prefers nushell, falls back to native.
pub struct Shell {
    nushell: NushellShell,
    native: NativeShell,
}

impl Shell {
    /// Create a new `Shell`, auto-detecting nushell availability.
    pub async fn new() -> Self {
        let nushell = NushellShell::new().await;
        let native = NativeShell::new();

        Self { nushell, native }
    }

    /// Check if nushell is available on this system.
    pub fn has_nushell(&self) -> bool {
        self.nushell.is_available()
    }

    /// Run a command and return its stdout as a trimmed `String`.
    ///
    /// Returns an error if the command fails (non-zero exit code).
    pub async fn run(&self, command: &str) -> Result<String> {
        let output = self.execute(command).await?;
        if output.exit_code != 0 {
            return Err(athen_core::error::AthenError::Other(format!(
                "command '{}' failed with exit code {}: {}",
                command,
                output.exit_code,
                output.stderr.trim()
            )));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Run a command and check if it succeeded (zero exit code).
    pub async fn run_ok(&self, command: &str) -> Result<bool> {
        let output = self.execute(command).await?;
        Ok(output.exit_code == 0)
    }

    /// Check if a program exists on the system.
    pub async fn has_program(&self, program: &str) -> bool {
        matches!(self.which(program).await, Ok(Some(_)))
    }
}

#[async_trait]
impl ShellExecutor for Shell {
    /// Execute via nushell if available, native shell otherwise.
    async fn execute(&self, command: &str) -> Result<SandboxOutput> {
        debug!(command, has_nushell = self.has_nushell(), "shell execute");
        self.nushell.execute(command).await
    }

    /// Always use the native platform shell.
    async fn execute_native(&self, command: &str) -> Result<SandboxOutput> {
        self.native.execute_native(command).await
    }

    /// Locate a program, preferring nushell's which if available.
    async fn which(&self, program: &str) -> Result<Option<PathBuf>> {
        self.nushell.which(program).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shell_creation() {
        let shell = Shell::new().await;
        // Should always succeed, nushell presence is optional.
        let _ = shell.has_nushell();
    }

    #[tokio::test]
    async fn test_shell_execute() {
        let shell = Shell::new().await;
        let output = shell.execute("echo unified").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.trim().contains("unified"));
    }

    #[tokio::test]
    async fn test_shell_execute_native() {
        let shell = Shell::new().await;
        let output = shell.execute_native("echo native_test").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "native_test");
    }

    #[tokio::test]
    async fn test_shell_run() {
        let shell = Shell::new().await;
        let result = shell.run("echo convenience").await.unwrap();
        assert_eq!(result, "convenience");
    }

    #[tokio::test]
    async fn test_shell_run_failure() {
        let shell = Shell::new().await;
        let result = shell.run("exit 1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shell_run_ok() {
        let shell = Shell::new().await;
        assert!(shell.run_ok("echo ok").await.unwrap());
        assert!(!shell.run_ok("exit 1").await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_has_program() {
        let shell = Shell::new().await;
        assert!(shell.has_program("sh").await);
        assert!(!shell.has_program("nonexistent_xyz_12345").await);
    }

    #[tokio::test]
    async fn test_shell_which() {
        let shell = Shell::new().await;
        let result = shell
            .which("nonexistent_program_xyz_12345")
            .await
            .unwrap();
        assert!(result.is_none());
    }
}
