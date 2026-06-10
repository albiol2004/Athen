//! Docker lifecycle for Athen instances (bollard).
//!
//! One container + one named volume per instance, all attached to a shared
//! bridge network with **no published ports** — inbound traffic can only
//! come from the panel (which resolves container IPs over the Docker API),
//! while outbound NAT stays available because instances must reach LLM
//! APIs, IMAP/SMTP and Telegram. The network is deliberately NOT
//! `internal: true`, which would cut instances off from the internet.
//!
//! The panel generates each instance's `ATHEN_HTTP_TOKEN` and injects it
//! at create time, so it never execs into containers to read tokens back.
//! Containers carry the `athen.panel.instance` label; status listing
//! filters on it.

use std::collections::HashMap;

use anyhow::Context;
use bollard::models::{
    ContainerCreateBody, HostConfig, NetworkCreateRequest, RestartPolicy, RestartPolicyNameEnum,
    VolumeCreateOptions,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
};
use bollard::Docker;
use futures::{Stream, StreamExt};

/// Label stamped on every panel-managed container; value = instance id.
pub const INSTANCE_LABEL: &str = "athen.panel.instance";

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

    /// Whether the daemon behind the socket runs rootless (Docker rootless
    /// mode or rootless Podman). A rootful socket is root-equivalent on
    /// the host — the caller surfaces that loudly rather than refusing to
    /// run, because single-operator rootful setups are still legitimate.
    pub async fn socket_rootless(&self) -> anyhow::Result<bool> {
        let info = self.docker.info().await.context("docker info")?;
        Ok(info
            .security_options
            .unwrap_or_default()
            .iter()
            .any(|o| o.contains("rootless")))
    }

    /// Create the shared bridge network if it doesn't exist (idempotent).
    ///
    /// Created with inter-container communication (ICC) disabled: the
    /// bridge only exists so the panel (host) can reach instances and
    /// instances can reach the internet — one tenant's container must not
    /// be able to probe another's :8787. ICC=false drops bridge-internal
    /// forwarding while leaving host→container and NAT egress untouched.
    pub async fn ensure_network(&self, name: &str) -> anyhow::Result<()> {
        if let Ok(existing) = self
            .docker
            .inspect_network(
                name,
                None::<bollard::query_parameters::InspectNetworkOptions>,
            )
            .await
        {
            // Network options are immutable after create — a pre-ICC
            // network keeps working but without tenant isolation. Warn
            // loudly; the fix is a one-time recreate while no instance
            // containers are running.
            let icc_disabled = existing
                .options
                .as_ref()
                .and_then(|o| o.get("com.docker.network.bridge.enable_icc"))
                .map(|v| v == "false")
                .unwrap_or(false);
            if !icc_disabled {
                tracing::warn!(
                    network = name,
                    "network predates ICC isolation — instances can reach each other's \
                     token-gated ports. To fix: stop all instances, `docker network rm {name}`, \
                     restart the panel (it recreates the network with ICC disabled)."
                );
            }
            return Ok(());
        }
        self.docker
            .create_network(NetworkCreateRequest {
                name: name.to_string(),
                driver: Some("bridge".to_string()),
                options: Some(HashMap::from([(
                    "com.docker.network.bridge.enable_icc".to_string(),
                    "false".to_string(),
                )])),
                ..Default::default()
            })
            .await
            .with_context(|| format!("creating docker network {name}"))?;
        Ok(())
    }

    /// Create volume + container for a new instance. `env` is the full
    /// environment (token, addr, operator-provided secrets). The container
    /// is created but not started. `memory_mb`/`cpus` become hard cgroup
    /// limits (`None` = unlimited).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_instance(
        &self,
        instance_id: &str,
        container_name: &str,
        volume_name: &str,
        image: &str,
        network: &str,
        env: Vec<String>,
        memory_mb: Option<u64>,
        cpus: Option<f64>,
    ) -> anyhow::Result<()> {
        self.ensure_network(network).await?;
        self.docker
            .create_volume(VolumeCreateOptions {
                name: Some(volume_name.to_string()),
                labels: Some(HashMap::from([(
                    INSTANCE_LABEL.to_string(),
                    instance_id.to_string(),
                )])),
                ..Default::default()
            })
            .await
            .with_context(|| format!("creating volume {volume_name}"))?;

        let body = ContainerCreateBody {
            image: Some(image.to_string()),
            env: Some(env),
            labels: Some(HashMap::from([(
                INSTANCE_LABEL.to_string(),
                instance_id.to_string(),
            )])),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{volume_name}:/data")]),
                network_mode: Some(network.to_string()),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                memory: memory_mb.map(|mb| (mb * 1024 * 1024) as i64),
                // swap == memory disables swap: a runaway instance gets
                // OOM-killed (and restarted by the policy) instead of
                // grinding the whole host into swap death.
                memory_swap: memory_mb.map(|mb| (mb * 1024 * 1024) as i64),
                nano_cpus: cpus.map(|c| (c * 1e9) as i64),
                // Privilege hardening: the image runs as uid 1000 with the
                // daemon binary as direct entrypoint, so it needs no
                // capabilities and must never gain any (setuid binaries
                // included). Shrinks what a compromised instance can do —
                // especially relevant when the panel drives a rootful
                // socket. pids_limit is a fork-bomb backstop, sized well
                // above the daemon + portable-Python subprocess peak.
                cap_drop: Some(vec!["ALL".to_string()]),
                security_opt: Some(vec!["no-new-privileges:true".to_string()]),
                pids_limit: Some(2048),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::new()
                        .name(container_name)
                        .build(),
                ),
                body,
            )
            .await
            .with_context(|| format!("creating container {container_name}"))?;
        Ok(())
    }

    pub async fn start(&self, container_name: &str) -> anyhow::Result<()> {
        self.docker
            .start_container(
                container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .with_context(|| format!("starting {container_name}"))?;
        Ok(())
    }

    /// Graceful stop (SIGTERM, 30s) — matches the daemon's drain path.
    pub async fn stop(&self, container_name: &str) -> anyhow::Result<()> {
        self.docker
            .stop_container(
                container_name,
                Some(StopContainerOptionsBuilder::new().t(30).build()),
            )
            .await
            .with_context(|| format!("stopping {container_name}"))?;
        Ok(())
    }

    /// Force-remove the container; optionally its data volume too.
    pub async fn remove(
        &self,
        container_name: &str,
        volume_name: &str,
        delete_data: bool,
    ) -> anyhow::Result<()> {
        match self
            .docker
            .remove_container(
                container_name,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await
        {
            Ok(()) => {}
            // 404 = already gone; removal must stay idempotent so a
            // half-deleted instance can be cleaned up by retrying.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(e) => return Err(e).with_context(|| format!("removing {container_name}")),
        }
        if delete_data {
            match self
                .docker
                .remove_volume(
                    volume_name,
                    None::<bollard::query_parameters::RemoveVolumeOptions>,
                )
                .await
            {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Err(e) => return Err(e).with_context(|| format!("removing volume {volume_name}")),
            }
        }
        Ok(())
    }

    /// Map container name → (state, human status) for every panel-managed
    /// container, running or not.
    pub async fn status_by_container(&self) -> anyhow::Result<HashMap<String, (String, String)>> {
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&HashMap::from([(
                "label".to_string(),
                vec![INSTANCE_LABEL.to_string()],
            )]))
            .build();
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .context("listing containers")?;
        let mut out = HashMap::new();
        for c in list {
            let Some(name) = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
            else {
                continue;
            };
            let state = c
                .state
                .map(|s| s.to_string().to_lowercase())
                .unwrap_or_else(|| "unknown".into());
            out.insert(name, (state, c.status.unwrap_or_default()));
        }
        Ok(out)
    }

    /// Volume name → used bytes, from one `docker system df` sweep scoped
    /// to volumes. Only the `local` driver reports sizes; volumes with an
    /// unknown size (-1) are omitted. Used by the disk-quota sweep — disk
    /// quotas are SOFT (measure + warn) because Docker's hard quota
    /// (`storage_opt: size=`) only works on xfs-with-pquota backing
    /// filesystems and fails container creation everywhere else.
    pub async fn volume_usage_bytes(&self) -> anyhow::Result<HashMap<String, u64>> {
        // No `type=volume` filter: bollard can't URL-encode the Vec
        // filter param ("Unable to URLEncode" on every daemon), so we
        // take the full df payload and read just the volumes section.
        let usage = self
            .docker
            .df(None::<bollard::query_parameters::DataUsageOptions>)
            .await
            .context("docker df")?;
        let mut out = HashMap::new();
        for v in usage.volumes.unwrap_or_default() {
            if let Some(u) = v.usage_data {
                if u.size >= 0 {
                    out.insert(v.name, u.size as u64);
                }
            }
        }
        Ok(out)
    }

    /// Resolve the container's IP on `network`. Looked up per proxy
    /// request rather than stored: container IPs change across restarts,
    /// and the panel (running on the host) can't use Docker DNS names.
    pub async fn instance_ip(&self, container_name: &str, network: &str) -> anyhow::Result<String> {
        let info = self
            .docker
            .inspect_container(
                container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .with_context(|| format!("inspecting {container_name}"))?;
        let ip = info
            .network_settings
            .and_then(|ns| ns.networks)
            .and_then(|nets| nets.get(network).cloned())
            .and_then(|net| net.ip_address)
            .filter(|ip| !ip.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "container {container_name} has no IP on network {network} (not running?)"
                )
            })?;
        Ok(ip)
    }

    /// Upload small text files into a (created, not necessarily running)
    /// container — used to seed `config.toml` / `models.toml` into the
    /// instance's `/data` volume before first start.
    pub async fn upload_files(
        &self,
        container_name: &str,
        dest: &str,
        files: &[(String, String)],
    ) -> anyhow::Result<()> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, content) in files {
            let bytes = content.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            // The athen image runs as uid/gid 1000 (`useradd --uid 1000
            // athen` in the Dockerfile); without this the files extract
            // as root:root 0600 and the daemon can't read its own config.
            header.set_uid(1000);
            header.set_gid(1000);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes)?;
        }
        let archive = builder.into_inner()?;
        self.docker
            .upload_to_container(
                container_name,
                Some(
                    bollard::query_parameters::UploadToContainerOptionsBuilder::new()
                        .path(dest)
                        .build(),
                ),
                bollard::body_full(archive.into()),
            )
            .await
            .with_context(|| format!("uploading seed files to {container_name}:{dest}"))?;
        Ok(())
    }

    /// Stream log lines (stdout+stderr interleaved). `follow` keeps the
    /// stream open for live tailing.
    pub fn logs(
        &self,
        container_name: &str,
        tail: u32,
        follow: bool,
    ) -> impl Stream<Item = String> + use<> {
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .tail(&tail.to_string())
            .follow(follow)
            .build();
        self.docker
            .logs(container_name, Some(opts))
            .map(|item| match item {
                Ok(line) => String::from_utf8_lossy(&line.into_bytes()).into_owned(),
                Err(e) => format!("[log stream error: {e}]"),
            })
    }
}
