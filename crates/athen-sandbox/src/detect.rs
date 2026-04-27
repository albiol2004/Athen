//! Auto-detect available sandboxing capabilities on the current platform.

use athen_core::sandbox::SandboxCapabilities;
use tokio::process::Command;
use tracing::debug;

/// Detects what sandboxing mechanisms are available on the current system.
pub struct SandboxDetector;

impl SandboxDetector {
    /// Detect what sandboxing is available on the current system.
    pub async fn detect() -> SandboxCapabilities {
        let (bubblewrap, landlock, podman, docker) = tokio::join!(
            Self::has_bwrap(),
            Self::has_landlock(),
            Self::has_podman(),
            Self::has_docker(),
        );

        let macos_sandbox = Self::has_macos_sandbox();
        let windows_sandbox = Self::has_windows_sandbox();

        let caps = SandboxCapabilities {
            bubblewrap,
            landlock,
            macos_sandbox,
            windows_sandbox,
            podman,
            docker,
        };

        debug!(?caps, "Detected sandbox capabilities");
        caps
    }

    /// Check if the `bwrap` (bubblewrap) binary is available in PATH.
    /// Only meaningful on Linux.
    async fn has_bwrap() -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        command_exists("bwrap").await
    }

    /// Check if Landlock LSM is available.
    /// Requires Linux kernel >= 5.13 and landlock listed in active LSMs.
    async fn has_landlock() -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        Self::check_landlock_support().await
    }

    async fn check_landlock_support() -> bool {
        // Check if /sys/kernel/security/lsm contains "landlock"
        match tokio::fs::read_to_string("/sys/kernel/security/lsm").await {
            Ok(content) => {
                let has_it = content.contains("landlock");
                debug!(has_landlock = has_it, lsm_content = %content.trim(), "Checked Landlock LSM");
                has_it
            }
            Err(e) => {
                debug!(?e, "Could not read /sys/kernel/security/lsm");
                false
            }
        }
    }

    /// Check if `sandbox-exec` is available (macOS only).
    fn has_macos_sandbox() -> bool {
        #[cfg(target_os = "macos")]
        {
            crate::macos::macos_capability()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Check if a Windows sandbox primitive is available (Windows only).
    fn has_windows_sandbox() -> bool {
        #[cfg(target_os = "windows")]
        {
            crate::windows::windows_capability()
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    /// Check if Podman is installed and runnable.
    async fn has_podman() -> bool {
        command_succeeds("podman", &["--version"]).await
    }

    /// Check if Docker is installed and runnable.
    async fn has_docker() -> bool {
        command_succeeds("docker", &["--version"]).await
    }
}

/// Check if a command binary exists in PATH by running `which` / looking it up.
async fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if a command runs successfully (exit code 0).
async fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_detect_returns_capabilities() {
        let caps = SandboxDetector::detect().await;
        // We can't assert specific values since they depend on the host,
        // but we can verify the struct is populated without panicking.
        let _ = caps.bubblewrap;
        let _ = caps.landlock;
        let _ = caps.macos_sandbox;
        let _ = caps.windows_sandbox;
        let _ = caps.podman;
        let _ = caps.docker;
    }

    #[test]
    fn test_macos_sandbox_false_on_linux() {
        // On Linux, macOS sandbox should always be false
        assert!(!SandboxDetector::has_macos_sandbox());
    }

    #[test]
    fn test_windows_sandbox_false_on_linux() {
        // On Linux, Windows sandbox should always be false
        assert!(!SandboxDetector::has_windows_sandbox());
    }

    #[tokio::test]
    async fn test_command_exists_with_known_binary() {
        // `ls` should exist on any Linux/macOS system
        assert!(command_exists("ls").await);
    }

    #[tokio::test]
    async fn test_command_exists_with_nonexistent_binary() {
        assert!(!command_exists("definitely_not_a_real_binary_12345").await);
    }

    #[tokio::test]
    async fn test_command_succeeds_with_known_command() {
        assert!(command_succeeds("true", &[]).await);
    }

    #[tokio::test]
    async fn test_command_succeeds_with_failing_command() {
        assert!(!command_succeeds("false", &[]).await);
    }
}
