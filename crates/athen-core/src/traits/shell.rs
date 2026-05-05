use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::Result;
use crate::traits::sandbox::SandboxOutput;

/// Extra env vars and working directory to apply to a shell command via the
/// OS process API. Lets callers configure the execution environment without
/// having to embed shell-specific syntax (e.g. `export X=Y && ...`) in the
/// command string, which is portable across sh, bash, nushell, cmd, pwsh.
#[derive(Default)]
pub struct ShellOptions<'a> {
    /// Additional env vars to merge into the spawned process's environment.
    /// Order matters for PATH-like values: the caller is responsible for
    /// composing the final string with the correct OS-specific separator
    /// (use [`std::env::join_paths`]).
    pub env: &'a [(String, String)],
    /// Working directory for the spawned process. `None` keeps the parent
    /// process's cwd.
    pub cwd: Option<&'a Path>,
}

/// Cross-platform shell execution.
/// Primary: embedded Nushell. Fallback: native platform shell.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Execute a command in the cross-platform shell (Nushell).
    async fn execute(&self, command: &str) -> Result<SandboxOutput>;

    /// Execute a command in the native platform shell (bash/zsh/pwsh).
    async fn execute_native(&self, command: &str) -> Result<SandboxOutput>;

    /// Check if a command/program is available on the system.
    async fn which(&self, program: &str) -> Result<Option<PathBuf>>;

    /// Execute a command with extra env vars and cwd applied via the OS
    /// process API. Default impl ignores `opts` and delegates to [`execute`];
    /// concrete adapters should override to honor the options.
    async fn execute_with(
        &self,
        command: &str,
        _opts: ShellOptions<'_>,
    ) -> Result<SandboxOutput> {
        self.execute(command).await
    }
}
