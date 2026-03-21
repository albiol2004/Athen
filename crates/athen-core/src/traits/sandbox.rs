use async_trait::async_trait;

use crate::error::Result;
use crate::sandbox::{SandboxCapabilities, SandboxLevel};

/// Manages sandboxed execution of commands and scripts.
#[async_trait]
pub trait SandboxExecutor: Send + Sync {
    /// Detect what sandboxing is available on this system.
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities>;

    /// Execute a command within the specified sandbox level.
    async fn execute(
        &self,
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput>;
}

#[derive(Debug, Clone)]
pub struct SandboxOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub execution_time_ms: u64,
}
