//! MCP Filesystem server binary.
//!
//! Standalone JSON-RPC server speaking the MCP protocol over stdio.
//! Usage: `mcp-filesystem <SANDBOX_ROOT>`
//! All filesystem operations are confined to `SANDBOX_ROOT`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use mcp_filesystem::Filesystem;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: mcp-filesystem <SANDBOX_ROOT>")?;

    let fs = Filesystem::new(root.clone())
        .with_context(|| format!("failed to open sandbox root {}", root.display()))?;

    tracing::info!(root = %fs.root().display(), "starting mcp-filesystem");

    let service = fs.serve(stdio()).await.inspect_err(|e| {
        tracing::error!(error = %e, "serve failed");
    })?;
    service.waiting().await?;
    Ok(())
}
