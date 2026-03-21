//! Sandboxing for Athen.
//!
//! Tiered isolation: OS-native (bwrap/landlock/sandbox-exec) for most cases,
//! container (Podman/Docker) for critical operations.

pub mod container;
pub mod detect;

#[cfg(target_os = "linux")]
pub mod bwrap;

#[cfg(target_os = "linux")]
pub mod landlock;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel};
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};
use std::time::Instant;
use tracing::{debug, info};

use crate::container::ContainerExecutor;
use crate::detect::SandboxDetector;

/// Unified sandbox facade that auto-detects capabilities and selects
/// the best available sandbox for each execution.
pub struct UnifiedSandbox {
    capabilities: SandboxCapabilities,
}

impl UnifiedSandbox {
    /// Create a new sandbox, auto-detecting capabilities.
    pub async fn new() -> Result<Self> {
        let capabilities = SandboxDetector::detect().await;
        info!(?capabilities, "Initialized UnifiedSandbox");
        Ok(Self { capabilities })
    }

    /// Create with pre-determined capabilities (useful for testing).
    pub fn with_capabilities(capabilities: SandboxCapabilities) -> Self {
        Self { capabilities }
    }

    /// Get the detected capabilities.
    pub fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }

    /// Select the best sandbox strategy name for a given level.
    /// Returns a string describing which sandbox will be used.
    pub fn select_sandbox_name(&self, level: &SandboxLevel) -> Result<&'static str> {
        match level {
            SandboxLevel::None => Ok("none"),
            SandboxLevel::OsNative { .. } => {
                if cfg!(target_os = "linux") && self.capabilities.bubblewrap {
                    Ok("bwrap")
                } else if cfg!(target_os = "linux") && self.capabilities.landlock {
                    Ok("landlock")
                } else if cfg!(target_os = "macos") && self.capabilities.macos_sandbox {
                    Ok("macos-sandbox")
                } else if cfg!(target_os = "windows") && self.capabilities.windows_sandbox {
                    Ok("windows-sandbox")
                } else {
                    Err(AthenError::Sandbox(
                        "No OS-native sandbox available on this system".into(),
                    ))
                }
            }
            SandboxLevel::Container { .. } => {
                if self.capabilities.podman {
                    Ok("podman")
                } else if self.capabilities.docker {
                    Ok("docker")
                } else {
                    Err(AthenError::Sandbox(
                        "No container runtime available (need podman or docker)".into(),
                    ))
                }
            }
        }
    }

    /// Execute with automatic sandbox selection based on the sandbox level.
    pub async fn execute_sandboxed(
        &self,
        command: &str,
        args: &[&str],
        level: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let sandbox_name = self.select_sandbox_name(level)?;
        debug!(sandbox = sandbox_name, command, "Executing sandboxed command");

        match level {
            SandboxLevel::None => Self::execute_direct(command, args).await,

            SandboxLevel::OsNative { .. } => {
                self.execute_os_native(command, args, level).await
            }

            SandboxLevel::Container { .. } => {
                self.execute_container(command, args, level).await
            }
        }
    }

    /// Execute a command directly without any sandboxing.
    async fn execute_direct(command: &str, args: &[&str]) -> Result<SandboxOutput> {
        debug!(command, "Executing command without sandbox");
        let start = Instant::now();

        let output = tokio::process::Command::new(command)
            .args(args)
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("Direct execution failed: {e}")))?;

        let elapsed = start.elapsed();

        Ok(SandboxOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            execution_time_ms: elapsed.as_millis() as u64,
        })
    }

    /// Execute using OS-native sandbox.
    async fn execute_os_native(
        &self,
        command: &str,
        args: &[&str],
        level: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        #[cfg(target_os = "linux")]
        {
            if self.capabilities.bubblewrap {
                return bwrap::BwrapSandbox::execute(command, args, level).await;
            }
            if self.capabilities.landlock {
                let sandbox = landlock::LandlockSandbox;
                return sandbox.execute(command, args, level).await;
            }
        }

        // macOS and Windows are handled via cfg-gated modules that won't
        // compile on other platforms, so we fall through to the error.
        #[cfg(target_os = "macos")]
        {
            if self.capabilities.macos_sandbox {
                let sandbox = macos::MacOsSandbox;
                return sandbox.execute(command, args, level).await;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.capabilities.windows_sandbox {
                let sandbox = windows::WindowsSandbox;
                return sandbox.execute(command, args, level).await;
            }
        }

        Err(AthenError::Sandbox(
            "No OS-native sandbox available".into(),
        ))
    }

    /// Execute using container runtime.
    async fn execute_container(
        &self,
        command: &str,
        args: &[&str],
        level: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let executor = if self.capabilities.podman {
            ContainerExecutor::with_runtime(container::ContainerRuntime::Podman)
        } else if self.capabilities.docker {
            ContainerExecutor::with_runtime(container::ContainerRuntime::Docker)
        } else {
            return Err(AthenError::Sandbox(
                "No container runtime available".into(),
            ));
        };

        executor.execute(command, args, level).await
    }
}

#[async_trait]
impl SandboxExecutor for UnifiedSandbox {
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities> {
        Ok(self.capabilities.clone())
    }

    async fn execute(
        &self,
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        self.execute_sandboxed(command, args, sandbox).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::sandbox::SandboxProfile;

    fn all_false_caps() -> SandboxCapabilities {
        SandboxCapabilities {
            bubblewrap: false,
            landlock: false,
            macos_sandbox: false,
            windows_sandbox: false,
            podman: false,
            docker: false,
        }
    }

    fn linux_caps() -> SandboxCapabilities {
        SandboxCapabilities {
            bubblewrap: true,
            landlock: true,
            macos_sandbox: false,
            windows_sandbox: false,
            podman: true,
            docker: true,
        }
    }

    #[test]
    fn test_select_sandbox_none() {
        let sandbox = UnifiedSandbox::with_capabilities(all_false_caps());
        let result = sandbox.select_sandbox_name(&SandboxLevel::None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "none");
    }

    #[test]
    fn test_select_sandbox_os_native_bwrap_preferred() {
        let sandbox = UnifiedSandbox::with_capabilities(linux_caps());
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::ReadOnly,
        };
        let result = sandbox.select_sandbox_name(&level);
        // On Linux, bwrap should be preferred over landlock
        if cfg!(target_os = "linux") {
            assert_eq!(result.unwrap(), "bwrap");
        }
    }

    #[test]
    fn test_select_sandbox_os_native_landlock_fallback() {
        let caps = SandboxCapabilities {
            bubblewrap: false,
            landlock: true,
            macos_sandbox: false,
            windows_sandbox: false,
            podman: false,
            docker: false,
        };
        let sandbox = UnifiedSandbox::with_capabilities(caps);
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::ReadOnly,
        };
        let result = sandbox.select_sandbox_name(&level);
        if cfg!(target_os = "linux") {
            assert_eq!(result.unwrap(), "landlock");
        }
    }

    #[test]
    fn test_select_sandbox_os_native_none_available() {
        let sandbox = UnifiedSandbox::with_capabilities(all_false_caps());
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::ReadOnly,
        };
        let result = sandbox.select_sandbox_name(&level);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_sandbox_container_podman_preferred() {
        let sandbox = UnifiedSandbox::with_capabilities(linux_caps());
        let level = SandboxLevel::Container {
            image: "alpine:latest".to_string(),
            mounts: vec![],
            network: false,
        };
        let result = sandbox.select_sandbox_name(&level);
        assert_eq!(result.unwrap(), "podman");
    }

    #[test]
    fn test_select_sandbox_container_docker_fallback() {
        let caps = SandboxCapabilities {
            bubblewrap: false,
            landlock: false,
            macos_sandbox: false,
            windows_sandbox: false,
            podman: false,
            docker: true,
        };
        let sandbox = UnifiedSandbox::with_capabilities(caps);
        let level = SandboxLevel::Container {
            image: "alpine:latest".to_string(),
            mounts: vec![],
            network: false,
        };
        let result = sandbox.select_sandbox_name(&level);
        assert_eq!(result.unwrap(), "docker");
    }

    #[test]
    fn test_select_sandbox_container_none_available() {
        let sandbox = UnifiedSandbox::with_capabilities(all_false_caps());
        let level = SandboxLevel::Container {
            image: "alpine:latest".to_string(),
            mounts: vec![],
            network: false,
        };
        let result = sandbox.select_sandbox_name(&level);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_direct_succeeds() {
        let result = UnifiedSandbox::execute_direct("echo", &["hello"]).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn test_execute_sandboxed_none_level() {
        let sandbox = UnifiedSandbox::with_capabilities(all_false_caps());
        let result = sandbox
            .execute_sandboxed("echo", &["test"], &SandboxLevel::None)
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "test");
    }

    #[tokio::test]
    async fn test_detect_capabilities_trait() {
        let sandbox = UnifiedSandbox::with_capabilities(linux_caps());
        let caps = sandbox.detect_capabilities().await.unwrap();
        assert!(caps.bubblewrap);
        assert!(caps.podman);
    }

    #[test]
    fn test_capabilities_accessor() {
        let caps = linux_caps();
        let sandbox = UnifiedSandbox::with_capabilities(caps.clone());
        assert_eq!(sandbox.capabilities().bubblewrap, caps.bubblewrap);
        assert_eq!(sandbox.capabilities().docker, caps.docker);
    }
}
