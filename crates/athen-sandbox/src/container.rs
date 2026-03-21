//! Podman/Docker container execution fallback.

use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{Mount, SandboxLevel};
use athen_core::traits::sandbox::SandboxOutput;
use std::time::Instant;
use tokio::process::Command;
use tracing::{debug, warn};

/// Which container runtime to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntime {
    Podman,
    Docker,
}

impl ContainerRuntime {
    /// Return the binary name for this runtime.
    pub fn binary(&self) -> &'static str {
        match self {
            ContainerRuntime::Podman => "podman",
            ContainerRuntime::Docker => "docker",
        }
    }
}

/// Executes commands inside ephemeral containers using Podman or Docker.
pub struct ContainerExecutor {
    runtime: ContainerRuntime,
}

impl ContainerExecutor {
    /// Create a new executor, auto-detecting Podman first, then Docker.
    pub async fn new() -> Result<Self> {
        if Self::runtime_available(ContainerRuntime::Podman).await {
            debug!("Using Podman container runtime");
            Ok(Self {
                runtime: ContainerRuntime::Podman,
            })
        } else if Self::runtime_available(ContainerRuntime::Docker).await {
            debug!("Using Docker container runtime");
            Ok(Self {
                runtime: ContainerRuntime::Docker,
            })
        } else {
            Err(AthenError::Sandbox(
                "No container runtime available (tried podman, docker)".into(),
            ))
        }
    }

    /// Create an executor with a specific runtime.
    pub fn with_runtime(runtime: ContainerRuntime) -> Self {
        Self { runtime }
    }

    /// Check whether a runtime binary is available.
    async fn runtime_available(runtime: ContainerRuntime) -> bool {
        Command::new(runtime.binary())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Check if this executor's runtime is available.
    pub async fn is_available(&self) -> bool {
        Self::runtime_available(self.runtime).await
    }

    /// Get the container runtime being used.
    pub fn runtime(&self) -> ContainerRuntime {
        self.runtime
    }

    /// Pull an image to ensure it is available locally.
    pub async fn pull_image(&self, image: &str) -> Result<()> {
        debug!(runtime = ?self.runtime, image, "Pulling container image");
        let output = Command::new(self.runtime.binary())
            .args(["pull", image])
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("Failed to pull image: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AthenError::Sandbox(format!(
                "Failed to pull image {image}: {stderr}"
            )));
        }
        Ok(())
    }

    /// Build the argument list for a container run command.
    /// This is public for testability.
    #[allow(clippy::too_many_arguments)]
    pub fn build_run_args(
        &self,
        command: &str,
        args: &[&str],
        image: &str,
        mounts: &[Mount],
        network: bool,
        memory_limit: Option<&str>,
        cpu_limit: Option<f64>,
        timeout_secs: Option<u64>,
    ) -> Vec<String> {
        let mut run_args = vec!["run".to_string(), "--rm".to_string()];

        // Network isolation
        if !network {
            run_args.push("--network=none".to_string());
        }

        // Volume mounts
        for mount in mounts {
            let host = mount.host_path.display();
            let container = mount.container_path.display();
            if mount.read_only {
                run_args.push("-v".to_string());
                run_args.push(format!("{host}:{container}:ro"));
            } else {
                run_args.push("-v".to_string());
                run_args.push(format!("{host}:{container}"));
            }
        }

        // Resource limits
        if let Some(mem) = memory_limit {
            run_args.push("--memory".to_string());
            run_args.push(mem.to_string());
        }

        if let Some(cpus) = cpu_limit {
            run_args.push("--cpus".to_string());
            run_args.push(format!("{cpus}"));
        }

        // Timeout (Podman supports --timeout, Docker does not natively)
        if let Some(secs) = timeout_secs {
            if self.runtime == ContainerRuntime::Podman {
                run_args.push("--timeout".to_string());
                run_args.push(format!("{secs}"));
            }
            // For Docker, timeout would need to be handled externally
        }

        // Image
        run_args.push(image.to_string());

        // Command and arguments
        run_args.push(command.to_string());
        for arg in args {
            run_args.push(arg.to_string());
        }

        run_args
    }

    /// Execute a command inside a container, based on the SandboxLevel::Container variant.
    pub async fn execute(
        &self,
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let (image, mounts, network) = match sandbox {
            SandboxLevel::Container {
                image,
                mounts,
                network,
            } => (image.as_str(), mounts.as_slice(), *network),
            _ => {
                return Err(AthenError::Sandbox(
                    "ContainerExecutor requires SandboxLevel::Container".into(),
                ))
            }
        };

        let run_args = self.build_run_args(command, args, image, mounts, network, None, None, None);

        debug!(
            runtime = ?self.runtime,
            ?run_args,
            "Executing container command"
        );

        let start = Instant::now();
        let output = Command::new(self.runtime.binary())
            .args(&run_args)
            .output()
            .await
            .map_err(|e| AthenError::Sandbox(format!("Container execution failed: {e}")))?;

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
                "Container command exited with non-zero status"
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_executor(runtime: ContainerRuntime) -> ContainerExecutor {
        ContainerExecutor::with_runtime(runtime)
    }

    #[test]
    fn test_runtime_binary_names() {
        assert_eq!(ContainerRuntime::Podman.binary(), "podman");
        assert_eq!(ContainerRuntime::Docker.binary(), "docker");
    }

    #[test]
    fn test_build_run_args_basic() {
        let executor = make_executor(ContainerRuntime::Podman);
        let args = executor.build_run_args("echo", &["hello"], "alpine:latest", &[], true, None, None, None);
        assert_eq!(args, vec!["run", "--rm", "alpine:latest", "echo", "hello"]);
    }

    #[test]
    fn test_build_run_args_no_network() {
        let executor = make_executor(ContainerRuntime::Docker);
        let args = executor.build_run_args("ls", &[], "ubuntu:22.04", &[], false, None, None, None);
        assert!(args.contains(&"--network=none".to_string()));
    }

    #[test]
    fn test_build_run_args_mounts() {
        let executor = make_executor(ContainerRuntime::Podman);
        let mounts = vec![
            Mount {
                host_path: PathBuf::from("/home/user/data"),
                container_path: PathBuf::from("/data"),
                read_only: true,
            },
            Mount {
                host_path: PathBuf::from("/tmp/work"),
                container_path: PathBuf::from("/work"),
                read_only: false,
            },
        ];
        let args = executor.build_run_args("ls", &[], "alpine:latest", &mounts, true, None, None, None);
        assert!(args.contains(&"-v".to_string()));
        assert!(args.contains(&"/home/user/data:/data:ro".to_string()));
        assert!(args.contains(&"/tmp/work:/work".to_string()));
    }

    #[test]
    fn test_build_run_args_resource_limits() {
        let executor = make_executor(ContainerRuntime::Podman);
        let args = executor.build_run_args(
            "ls", &[], "alpine:latest", &[], true,
            Some("512m"), Some(2.0), None,
        );
        assert!(args.contains(&"--memory".to_string()));
        assert!(args.contains(&"512m".to_string()));
        assert!(args.contains(&"--cpus".to_string()));
        assert!(args.contains(&"2".to_string()));
    }

    #[test]
    fn test_build_run_args_timeout_podman() {
        let executor = make_executor(ContainerRuntime::Podman);
        let args = executor.build_run_args(
            "ls", &[], "alpine:latest", &[], true,
            None, None, Some(30),
        );
        assert!(args.contains(&"--timeout".to_string()));
        assert!(args.contains(&"30".to_string()));
    }

    #[test]
    fn test_build_run_args_timeout_docker_ignored() {
        let executor = make_executor(ContainerRuntime::Docker);
        let args = executor.build_run_args(
            "ls", &[], "alpine:latest", &[], true,
            None, None, Some(30),
        );
        // Docker does not support --timeout natively
        assert!(!args.contains(&"--timeout".to_string()));
    }

    #[test]
    fn test_build_run_args_multiple_command_args() {
        let executor = make_executor(ContainerRuntime::Podman);
        let args = executor.build_run_args(
            "bash", &["-c", "echo hello world"], "alpine:latest", &[], true,
            None, None, None,
        );
        let last_three: Vec<&str> = args.iter().rev().take(3).map(|s| s.as_str()).collect();
        assert!(last_three.contains(&"bash"));
        assert!(last_three.contains(&"-c"));
        assert!(last_three.contains(&"echo hello world"));
    }
}
