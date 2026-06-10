//! Docker lifecycle for Athen instances (bollard).
//!
//! One container + one named volume per instance, all attached to a shared
//! *internal* network (no published ports — only the panel, which joins or
//! reaches that network, can talk to them). The panel generates each
//! instance's `ATHEN_HTTP_TOKEN` and injects it at create time, so it
//! never needs to exec into containers to read tokens back.

use bollard::Docker;

#[derive(Clone)]
pub struct DockerCtl {
    docker: Docker,
}

impl DockerCtl {
    /// Connect with bollard defaults (`DOCKER_HOST` honored, unix socket
    /// otherwise). Does not ping — the daemon may come up later; calls
    /// fail per-operation.
    pub fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_defaults()?;
        Ok(Self { docker })
    }

    pub fn inner(&self) -> &Docker {
        &self.docker
    }
}
