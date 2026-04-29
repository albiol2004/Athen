//! Linux: Landlock LSM sandboxing.
//!
//! Landlock is a Linux security module (available since kernel 5.13) that enables
//! unprivileged processes to restrict their own filesystem access. Unlike bwrap,
//! which uses namespaces, Landlock operates at the kernel level using the Landlock ABI.
//!
//! A full implementation would:
//! 1. Use `prctl(PR_SET_NO_NEW_PRIVS, 1)` to prevent privilege escalation
//! 2. Create a Landlock ruleset via `landlock_create_ruleset()` syscall
//! 3. Add rules for allowed filesystem paths via `landlock_add_rule()`
//! 4. Enforce the ruleset via `landlock_restrict_self()`
//!
//! This is a stub implementation. The actual syscall-based implementation
//! would require the `landlock` crate or direct `libc` syscalls.

#[cfg(target_os = "linux")]
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use athen_core::error::{AthenError, Result};
#[cfg(target_os = "linux")]
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel};
#[cfg(target_os = "linux")]
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};

/// Landlock LSM sandbox executor for Linux.
///
/// TODO: Implement using the Landlock ABI (syscalls: landlock_create_ruleset,
/// landlock_add_rule, landlock_restrict_self) or the `landlock` crate.
#[cfg(target_os = "linux")]
pub struct LandlockSandbox;

#[cfg(target_os = "linux")]
#[async_trait]
impl SandboxExecutor for LandlockSandbox {
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
            "Landlock not yet implemented".to_string(),
        ))
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_landlock_returns_not_implemented() {
        let sandbox = LandlockSandbox;
        let result = sandbox
            .execute(
                "ls",
                &[],
                &SandboxLevel::OsNative {
                    profile: athen_core::sandbox::SandboxProfile::ReadOnly,
                },
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Landlock not yet implemented"),
            "Expected 'not yet implemented' error, got: {msg}"
        );
    }
}
