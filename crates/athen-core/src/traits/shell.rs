use async_trait::async_trait;

use crate::error::Result;
use crate::traits::sandbox::SandboxOutput;

/// Cross-platform shell execution.
/// Primary: embedded Nushell. Fallback: native platform shell.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Execute a command in the cross-platform shell (Nushell).
    async fn execute(&self, command: &str) -> Result<SandboxOutput>;

    /// Execute a command in the native platform shell (bash/zsh/pwsh).
    async fn execute_native(&self, command: &str) -> Result<SandboxOutput>;

    /// Check if a command/program is available on the system.
    async fn which(&self, program: &str) -> Result<Option<std::path::PathBuf>>;
}
