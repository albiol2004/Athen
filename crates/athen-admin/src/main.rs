//! Athen admin panel + gateway.
//!
//! Control plane for hosting multiple Athen instances: provisions one
//! container per user over the Docker API, holds the per-instance HTTP
//! tokens, authenticates human users (sessions + argon2), and reverse-
//! proxies `/i/{instance}/api/*` to the right container with the bearer
//! token injected server-side. Instances are never exposed directly —
//! they live on an internal Docker network with no published ports; TLS
//! belongs to whatever fronts THIS binary (Caddy / cloudflared / nginx).
//!
//! Operator-facing by design (env vars + files), same stance as headless
//! mode. Configuration:
//!
//! | Env var | Meaning | Default |
//! |---|---|---|
//! | `ATHEN_ADMIN_ADDR` | listen address | `127.0.0.1:8800` |
//! | `ATHEN_ADMIN_DATA_DIR` | panel DB + state | `~/.athen-admin` |
//! | `ATHEN_ADMIN_PASSWORD` | bootstrap admin password (else generated + printed once) | unset |
//! | `ATHEN_ADMIN_IMAGE` | image for new instances | `athen` |
//! | `ATHEN_ADMIN_NETWORK` | internal Docker network name | `athen-net` |
//! | `ATHEN_ADMIN_AUDIT_RETENTION_DAYS` | prune audit rows older than this (0 = keep forever) | `90` |
//! | `ATHEN_ADMIN_DISK_ENFORCE` | `warn` = never stop over-quota instances (default escalates warn → stop) | `stop` |
//! | `ATHEN_ADMIN_DISK_SWEEP_SECS` | disk sweep interval (min 5; also the warn→stop grace period) | `300` |
//! | `DOCKER_HOST` | honored by bollard (unix socket default; point at a rootless Docker or Podman socket to de-privilege the panel — rootful sockets get a startup warning + audit row + dashboard banner) | unset |

mod api;
mod auth;
mod db;
mod disk;
mod docker;
mod instances;
mod notify;
mod proxy;
mod ratelimit;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

/// Shared state for every handler.
pub struct PanelState {
    pub db: db::Db,
    pub docker: docker::DockerCtl,
    /// reqwest client used by the reverse proxy (no global timeout: the
    /// chat endpoint long-polls for the duration of an agent turn).
    pub http: reqwest::Client,
    pub cfg: PanelConfig,
    pub login_throttle: ratelimit::LoginThrottle,
    pub buckets: ratelimit::UserBuckets,
    /// instance id → data-volume bytes, refreshed by the disk sweep.
    pub disk_usage: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// Whether the Docker socket is rootless, probed once at startup
    /// (`None` = probe failed / daemon was down). Rootful is surfaced as
    /// a startup warning, an audit row and a dashboard banner — it means
    /// this panel is root-equivalent on the host.
    pub socket_rootless: Option<bool>,
}

#[derive(Clone)]
pub struct PanelConfig {
    pub addr: SocketAddr,
    pub data_dir: PathBuf,
    pub instance_image: String,
    pub network: String,
}

impl PanelConfig {
    fn from_env() -> anyhow::Result<Self> {
        let addr: SocketAddr = std::env::var("ATHEN_ADMIN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8800".into())
            .parse()
            .context("ATHEN_ADMIN_ADDR is not a valid socket address")?;
        let data_dir = match std::env::var("ATHEN_ADMIN_DATA_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => dirs_home().join(".athen-admin"),
        };
        Ok(Self {
            addr,
            data_dir,
            instance_image: std::env::var("ATHEN_ADMIN_IMAGE").unwrap_or_else(|_| "athen".into()),
            network: std::env::var("ATHEN_ADMIN_NETWORK").unwrap_or_else(|_| "athen-net".into()),
        })
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Daily prune of audit rows older than `ATHEN_ADMIN_AUDIT_RETENTION_DAYS`
/// (default 90; `0` keeps everything forever). Each prune that removes
/// rows leaves its own audit entry, so the trail records its truncation.
fn spawn_audit_retention(db: db::Db) {
    let days: u32 = std::env::var("ATHEN_ADMIN_AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    if days == 0 {
        tracing::info!("audit retention disabled (keep forever)");
        return;
    }
    tokio::spawn(async move {
        loop {
            let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
            match db::audit_prune_before(&db, cutoff).await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(rows = n, days, "audit log pruned");
                    db::audit(
                        &db,
                        "system",
                        "audit_prune",
                        "",
                        &format!("{n} rows older than {days} days"),
                    )
                    .await;
                }
                Err(e) => tracing::warn!(error = %e, "audit prune failed"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = PanelConfig::from_env()?;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;

    let db = db::Db::open(&cfg.data_dir.join("panel.db")).context("opening panel database")?;
    auth::bootstrap_admin(&db).await?;

    let docker = docker::DockerCtl::connect().context("connecting to Docker daemon")?;

    // Probe socket posture once. A rootful socket makes this panel
    // root-equivalent on the host; we run anyway (single-operator rootful
    // setups are legitimate) but say so everywhere an operator looks.
    let socket_rootless = match docker.socket_rootless().await {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "could not probe docker socket posture");
            None
        }
    };
    if socket_rootless == Some(false) {
        tracing::warn!(
            "panel is driving a ROOTFUL Docker socket — anyone who compromises this \
             process is root on the host. Point DOCKER_HOST at a rootless Docker or \
             Podman socket (e.g. unix:///run/user/<uid>/podman/podman.sock)."
        );
        db::audit(
            &db,
            "system",
            "rootful_socket",
            "",
            "panel started against a root-equivalent Docker socket",
        )
        .await;
    }

    let state = Arc::new(PanelState {
        db,
        docker,
        http: reqwest::Client::builder()
            .build()
            .context("building proxy http client")?,
        cfg: cfg.clone(),
        login_throttle: ratelimit::LoginThrottle::default(),
        buckets: ratelimit::UserBuckets::default(),
        disk_usage: std::sync::Mutex::new(std::collections::HashMap::new()),
        socket_rootless,
    });

    // Forward approval-questions / urgent notifications from running
    // instances to users' notify webhooks (phones).
    notify::spawn(state.clone());
    // Measure data-volume usage + enforce disk quotas (warn, then stop).
    disk::spawn(state.clone());
    // Audit retention: the log is append-only, so prune old rows daily.
    spawn_audit_retention(state.db.clone());

    let app = api::router(state.clone());

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .with_context(|| format!("binding {}", cfg.addr))?;
    tracing::info!(addr = %cfg.addr, "athen-admin panel listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await?;
    Ok(())
}
