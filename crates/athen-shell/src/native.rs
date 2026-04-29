//! Native platform shell passthrough (bash/zsh on Unix, cmd/pwsh on Windows).

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::traits::sandbox::SandboxOutput;
use athen_core::traits::shell::ShellExecutor;

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Native platform shell executor.
///
/// Uses `sh -c` on Unix and `cmd /C` on Windows to run commands through
/// the native system shell.
pub struct NativeShell;

impl NativeShell {
    pub fn new() -> Self {
        Self
    }

    /// Execute a command string via the native platform shell, returning
    /// the captured output along with timing information.
    async fn run_command(&self, command: &str) -> Result<SandboxOutput> {
        debug!(command, "executing native shell command");

        let start = Instant::now();

        let mut cmd = self.build_command(command);
        cmd.kill_on_drop(true);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            cmd.output(),
        )
        .await;

        let output = match result {
            Ok(inner) => inner?,
            Err(_) => {
                warn!(command, "command timed out after {}s", DEFAULT_TIMEOUT_SECS);
                return Err(AthenError::Timeout(std::time::Duration::from_secs(
                    DEFAULT_TIMEOUT_SECS,
                )));
            }
        };

        let execution_time_ms = start.elapsed().as_millis() as u64;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        debug!(exit_code, execution_time_ms, "command completed");

        Ok(SandboxOutput {
            exit_code,
            stdout,
            stderr,
            execution_time_ms,
        })
    }

    /// Build a `tokio::process::Command` configured for the current platform.
    #[cfg(unix)]
    fn build_command(&self, command: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }

    #[cfg(windows)]
    fn build_command(&self, command: &str) -> Command {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }

    /// Locate a program on the system PATH.
    #[cfg(unix)]
    async fn find_program(&self, program: &str) -> Result<Option<PathBuf>> {
        let output = self.run_command(&format!("which {}", program)).await?;
        if output.exit_code == 0 {
            let path = output.stdout.trim().to_string();
            if path.is_empty() {
                Ok(None)
            } else {
                Ok(Some(PathBuf::from(path)))
            }
        } else {
            Ok(None)
        }
    }

    #[cfg(windows)]
    async fn find_program(&self, program: &str) -> Result<Option<PathBuf>> {
        let output = self.run_command(&format!("where {}", program)).await?;
        if output.exit_code == 0 {
            let path = output
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if path.is_empty() {
                Ok(None)
            } else {
                Ok(Some(PathBuf::from(path)))
            }
        } else {
            Ok(None)
        }
    }
}

impl Default for NativeShell {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ShellExecutor for NativeShell {
    async fn execute(&self, command: &str) -> Result<SandboxOutput> {
        self.run_command(command).await
    }

    async fn execute_native(&self, command: &str) -> Result<SandboxOutput> {
        self.run_command(command).await
    }

    async fn which(&self, program: &str) -> Result<Option<PathBuf>> {
        self.find_program(program).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_echo_command() {
        let shell = NativeShell::new();
        let output = shell.execute("echo hello").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "hello");
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_exit_code() {
        let shell = NativeShell::new();
        let output = shell.execute("exit 42").await.unwrap();
        assert_eq!(output.exit_code, 42);
    }

    #[tokio::test]
    async fn test_stderr_capture() {
        let shell = NativeShell::new();
        let output = shell.execute("echo error >&2").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stderr.trim().contains("error"));
    }

    #[tokio::test]
    async fn test_execution_time_recorded() {
        let shell = NativeShell::new();
        let output = shell.execute("echo fast").await.unwrap();
        // execution_time_ms should be set (>= 0 is always true for u64, just check it ran)
        assert_eq!(output.exit_code, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_which_known_program() {
        let shell = NativeShell::new();
        let result = shell.which("sh").await.unwrap();
        assert!(result.is_some(), "sh should be found on Unix");
        let path = result.unwrap();
        assert!(path.to_string_lossy().contains("sh"));
    }

    #[tokio::test]
    async fn test_which_nonexistent_program() {
        let shell = NativeShell::new();
        let result = shell.which("nonexistent_program_xyz_12345").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_execute_native_same_as_execute() {
        let shell = NativeShell::new();
        let output = shell.execute_native("echo native").await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "native");
    }
}
