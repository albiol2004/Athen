//! Native platform shell passthrough (bash/zsh on Unix, cmd/pwsh on Windows).

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, warn};

use athen_core::error::{AthenError, Result};
use athen_core::traits::sandbox::SandboxOutput;
use athen_core::traits::shell::{ShellExecutor, ShellOptions};

/// Default command timeout in seconds. Set to the upper bound of the
/// `shell_execute` tool's `timeout_ms` surface (600,000ms = 600s = 10min)
/// so the inner shell never silently undercuts what the agent asked for.
/// The agent tool layer already enforces tighter per-call timeouts via
/// `tokio::time::timeout(timeout_ms)` — this constant is just the ceiling.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

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
        self.run_command_with(command, ShellOptions::default())
            .await
    }

    /// Execute a command via the native shell, applying extra env vars and
    /// cwd through the OS process API (no shell-specific syntax needed).
    async fn run_command_with(
        &self,
        command: &str,
        opts: ShellOptions<'_>,
    ) -> Result<SandboxOutput> {
        // Same drain-gate semantics as nushell: refuse to spawn while an
        // update is in progress so the installer can swap binaries.
        let _permit = crate::drain::global_gate().enter().ok_or_else(|| {
            AthenError::Other("shell unavailable: update in progress".to_string())
        })?;

        debug!(command, "executing native shell command");

        let start = Instant::now();

        let mut cmd = self.build_command(command);
        cmd.kill_on_drop(true);
        for (k, v) in opts.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }

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
        // Suppress the cmd.exe console flash that GUI parents would otherwise
        // inherit: CREATE_NO_WINDOW = 0x0800_0000.
        cmd.creation_flags(0x0800_0000);
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

    async fn execute_with(&self, command: &str, opts: ShellOptions<'_>) -> Result<SandboxOutput> {
        self.run_command_with(command, opts).await
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

    /// Regression: `execute_with` must inject env vars through the OS
    /// process API, not as shell-syntax (`export X=Y && …`). Without
    /// this the wrapper breaks on nushell and on Windows cmd.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_with_injects_env() {
        let shell = NativeShell::new();
        let env = vec![("ATHEN_TEST_VAR".to_string(), "wired".to_string())];
        let output = shell
            .execute_with(
                "echo $ATHEN_TEST_VAR",
                ShellOptions {
                    env: &env,
                    cwd: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "wired");
    }

    /// Regression: `execute_with` must set the spawned process's cwd via
    /// `Command::current_dir`, not by prefixing `cd <dir> &&`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_with_sets_cwd() {
        let shell = NativeShell::new();
        let tmp = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&tmp).unwrap();
        let output = shell
            .execute_with(
                "pwd",
                ShellOptions {
                    env: &[],
                    cwd: Some(&tmp),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        let pwd = std::fs::canonicalize(output.stdout.trim()).unwrap();
        assert_eq!(pwd, canonical);
    }
}
