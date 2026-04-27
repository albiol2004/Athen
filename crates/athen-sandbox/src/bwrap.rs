//! Linux: bubblewrap (bwrap) sandboxing.
//!
//! Uses bubblewrap to create lightweight sandboxes with namespace isolation.
//! bwrap is commonly available on Linux systems (ships with Flatpak).

#[cfg(target_os = "linux")]
use athen_core::error::{AthenError, Result};
#[cfg(target_os = "linux")]
use athen_core::sandbox::{SandboxLevel, SandboxProfile};
#[cfg(target_os = "linux")]
use athen_core::traits::sandbox::SandboxOutput;
#[cfg(target_os = "linux")]
use std::time::Instant;
#[cfg(target_os = "linux")]
use tokio::process::Command;
#[cfg(target_os = "linux")]
use tracing::{debug, warn};

/// Bubblewrap-based sandbox executor for Linux.
#[cfg(target_os = "linux")]
pub struct BwrapSandbox;

#[cfg(target_os = "linux")]
impl BwrapSandbox {
    /// Build the bwrap argument list for a given sandbox profile.
    /// Public for testability.
    pub fn build_bwrap_args(
        command: &str,
        args: &[&str],
        profile: &SandboxProfile,
    ) -> Vec<String> {
        let mut bwrap_args: Vec<String> = Vec::new();

        // Always use these safety flags
        bwrap_args.push("--die-with-parent".to_string());
        bwrap_args.push("--new-session".to_string());

        match profile {
            SandboxProfile::ReadOnly => {
                // Mount the entire filesystem read-only
                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/".to_string());
                bwrap_args.push("/".to_string());

                // Mount /proc for process introspection
                bwrap_args.push("--proc".to_string());
                bwrap_args.push("/proc".to_string());

                // Mount /dev minimally
                bwrap_args.push("--dev".to_string());
                bwrap_args.push("/dev".to_string());
            }
            SandboxProfile::RestrictedWrite { allowed_paths } => {
                // Base filesystem is read-only
                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/".to_string());
                bwrap_args.push("/".to_string());

                // Mount allowed paths as read-write
                for path in allowed_paths {
                    let path_str = path.display().to_string();
                    bwrap_args.push("--bind".to_string());
                    bwrap_args.push(path_str.clone());
                    bwrap_args.push(path_str);
                }

                bwrap_args.push("--proc".to_string());
                bwrap_args.push("/proc".to_string());
                bwrap_args.push("--dev".to_string());
                bwrap_args.push("/dev".to_string());
            }
            SandboxProfile::NoNetwork => {
                // Full filesystem access but no network
                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/".to_string());
                bwrap_args.push("/".to_string());

                bwrap_args.push("--unshare-net".to_string());

                bwrap_args.push("--proc".to_string());
                bwrap_args.push("/proc".to_string());
                bwrap_args.push("--dev".to_string());
                bwrap_args.push("/dev".to_string());
            }
            SandboxProfile::Full => {
                // Maximum isolation: unshare everything
                bwrap_args.push("--unshare-all".to_string());

                // Minimal read-only bind mounts for execution
                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/usr".to_string());
                bwrap_args.push("/usr".to_string());

                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/lib".to_string());
                bwrap_args.push("/lib".to_string());

                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/lib64".to_string());
                bwrap_args.push("/lib64".to_string());

                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/bin".to_string());
                bwrap_args.push("/bin".to_string());

                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push("/sbin".to_string());
                bwrap_args.push("/sbin".to_string());

                // Writable tmpfs for /tmp
                bwrap_args.push("--tmpfs".to_string());
                bwrap_args.push("/tmp".to_string());

                bwrap_args.push("--proc".to_string());
                bwrap_args.push("/proc".to_string());
                bwrap_args.push("--dev".to_string());
                bwrap_args.push("/dev".to_string());
            }
        }

        // The command to execute inside the sandbox
        bwrap_args.push(command.to_string());
        for arg in args {
            bwrap_args.push(arg.to_string());
        }

        bwrap_args
    }

    /// Execute a command inside a bubblewrap sandbox.
    pub async fn execute(
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let profile = match sandbox {
            SandboxLevel::OsNative { profile } => profile,
            _ => {
                return Err(AthenError::Sandbox(
                    "BwrapSandbox requires SandboxLevel::OsNative".into(),
                ))
            }
        };

        let bwrap_args = Self::build_bwrap_args(command, args, profile);

        debug!(?bwrap_args, "Executing bwrap sandbox command");

        let start = Instant::now();
        let output = Command::new("bwrap")
            .args(&bwrap_args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("bwrap execution failed: {e}")))?;

        let elapsed = start.elapsed();

        let result = SandboxOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            execution_time_ms: elapsed.as_millis() as u64,
        };

        if !output.status.success() {
            warn!(
                exit_code = result.exit_code,
                stderr = %result.stderr,
                "bwrap command exited with non-zero status"
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_build_args_readonly() {
        let args = BwrapSandbox::build_bwrap_args("ls", &["-la"], &SandboxProfile::ReadOnly);

        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        assert!(args.contains(&"--ro-bind".to_string()));

        // Should NOT contain --bind (writable) or --unshare-net
        assert!(!args.contains(&"--bind".to_string()));
        assert!(!args.contains(&"--unshare-net".to_string()));
        assert!(!args.contains(&"--unshare-all".to_string()));

        // Command at the end
        let len = args.len();
        assert_eq!(args[len - 2], "ls");
        assert_eq!(args[len - 1], "-la");
    }

    #[test]
    fn test_build_args_restricted_write() {
        let profile = SandboxProfile::RestrictedWrite {
            allowed_paths: vec![
                PathBuf::from("/home/user/project"),
                PathBuf::from("/tmp/work"),
            ],
        };
        let args = BwrapSandbox::build_bwrap_args("cargo", &["build"], &profile);

        // Should have ro-bind for root
        assert!(args.contains(&"--ro-bind".to_string()));

        // Should have --bind for allowed paths
        assert!(args.contains(&"--bind".to_string()));
        assert!(args.contains(&"/home/user/project".to_string()));
        assert!(args.contains(&"/tmp/work".to_string()));

        // Command at the end
        let len = args.len();
        assert_eq!(args[len - 2], "cargo");
        assert_eq!(args[len - 1], "build");
    }

    #[test]
    fn test_build_args_no_network() {
        let args = BwrapSandbox::build_bwrap_args("curl", &["https://example.com"], &SandboxProfile::NoNetwork);

        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--ro-bind".to_string()));

        let len = args.len();
        assert_eq!(args[len - 2], "curl");
        assert_eq!(args[len - 1], "https://example.com");
    }

    #[test]
    fn test_build_args_full_isolation() {
        let args = BwrapSandbox::build_bwrap_args("python", &["script.py"], &SandboxProfile::Full);

        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        assert!(args.contains(&"--tmpfs".to_string()));

        // Should bind minimal paths: /usr, /lib, /lib64, /bin, /sbin
        assert!(args.contains(&"/usr".to_string()));
        assert!(args.contains(&"/lib".to_string()));
        assert!(args.contains(&"/lib64".to_string()));
        assert!(args.contains(&"/bin".to_string()));
        assert!(args.contains(&"/sbin".to_string()));

        let len = args.len();
        assert_eq!(args[len - 2], "python");
        assert_eq!(args[len - 1], "script.py");
    }

    #[test]
    fn test_build_args_always_has_safety_flags() {
        let profiles = vec![
            SandboxProfile::ReadOnly,
            SandboxProfile::NoNetwork,
            SandboxProfile::Full,
            SandboxProfile::RestrictedWrite {
                allowed_paths: vec![],
            },
        ];

        for profile in &profiles {
            let args = BwrapSandbox::build_bwrap_args("test", &[], profile);
            assert!(
                args.contains(&"--die-with-parent".to_string()),
                "Missing --die-with-parent for {profile:?}"
            );
            assert!(
                args.contains(&"--new-session".to_string()),
                "Missing --new-session for {profile:?}"
            );
        }
    }
}
