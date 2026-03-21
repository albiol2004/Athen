//! Windows: Job Objects / Windows Sandbox API.
//!
//! A full implementation would use Windows Job Objects to limit:
//! - Process creation (JOB_OBJECT_LIMIT_ACTIVE_PROCESS)
//! - Memory usage (JOB_OBJECT_LIMIT_PROCESS_MEMORY)
//! - CPU time (JOB_OBJECT_LIMIT_PROCESS_TIME)
//! - UI restrictions (JOB_OBJECT_UILIMIT_*)
//!
//! For higher isolation, Windows Sandbox (available on Windows 10 Pro+)
//! provides full VM-based isolation.

#[cfg(target_os = "windows")]
use async_trait::async_trait;
#[cfg(target_os = "windows")]
use athen_core::error::{AthenError, Result};
#[cfg(target_os = "windows")]
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel};
#[cfg(target_os = "windows")]
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};

/// Windows sandbox executor using Job Objects.
///
/// TODO: Implement using Windows Job Objects API via the `windows` crate.
#[cfg(target_os = "windows")]
pub struct WindowsSandbox;

#[cfg(target_os = "windows")]
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
            "Windows sandbox not yet implemented".to_string(),
        ))
    }
}
