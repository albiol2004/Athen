//! Windows sandbox backend stub.
//!
//! TODO: Implement using the `windows` crate with AppContainer + Job Object
//! isolation, restricting the process token via `SetTokenInformation`
//! (low-integrity SID, restricted SIDs) and applying `JOB_OBJECT_LIMIT_*`
//! flags for memory / CPU / process caps. Windows Sandbox (Win 10 Pro+) is
//! a heavier alternative for full VM-based isolation.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel};
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};

/// Returns true when a usable Windows sandbox primitive is available.
/// Always false until the AppContainer/Job Object backend is implemented.
pub fn windows_capability() -> bool {
    false
}

/// Windows sandbox executor.
///
/// Stub: refuses execution so the agent layer falls back to container
/// isolation or a denied operation rather than running unsandboxed.
pub struct WindowsSandbox;

#[async_trait]
impl SandboxExecutor for WindowsSandbox {
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
            "Windows AppContainer/Job Object backend is not yet implemented; refusing to run unsandboxed for safety".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_returns_false_for_now() {
        assert!(!windows_capability());
    }
}
