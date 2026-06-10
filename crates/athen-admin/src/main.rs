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
//! | `DOCKER_HOST` | honored by bollard (unix socket default) | unset |

mod api;
mod auth;
mod db;
mod docker;
mod proxy;
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

    let state = Arc::new(PanelState {
        db,
        docker,
        http: reqwest::Client::builder()
            .build()
            .context("building proxy http client")?,
        cfg: cfg.clone(),
    });

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
