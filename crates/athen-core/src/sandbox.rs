use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxLevel {
    /// No isolation (L1 read-only actions)
    None,
    /// OS-native (bwrap/landlock/sandbox-exec/Job Objects)
    OsNative { profile: SandboxProfile },
    /// Container (Podman/Docker)
    Container {
        image: String,
        mounts: Vec<Mount>,
        network: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxProfile {
    /// Read-only filesystem access
    ReadOnly,
    /// Read-write to specific paths only
    RestrictedWrite { allowed_paths: Vec<PathBuf> },
    /// Network-isolated execution
    NoNetwork,
    /// Full isolation (no fs, no network except explicit)
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

/// What sandboxing capabilities are available on this system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub bubblewrap: bool,
    pub landlock: bool,
    pub macos_sandbox: bool,
    pub windows_sandbox: bool,
    pub podman: bool,
    pub docker: bool,
}
