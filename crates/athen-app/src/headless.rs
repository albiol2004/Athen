//! Headless composition root — the full autonomous stack with no GUI.
//!
//! `athen --headless` (or `ATHEN_HEADLESS=1`) runs everything the desktop
//! app runs *except* the Tauri builder, the WebView, and the tray: config +
//! vault (+ env-var secret overlay), persistence, LLM router, coordinator,
//! sense monitors (email / calendar / Telegram), the autonomous dispatch
//! loop, the wake-up scheduler, and CalDAV sync — all on a plain tokio
//! runtime. Telegram is the user surface: owner messages drive the agent,
//! notifications and approval prompts arrive as bot messages with inline
//! keyboards.
//!
//! Per-instance isolation for containers / orchestration:
//! - `ATHEN_DATA_DIR` — private data tree (config, vault, SQLite, workspace)
//! - `ATHEN_VAULT_BACKEND=file` — skip the OS keychain entirely
//! - `ATHEN_*` secret env vars / `*_FILE` variants — see `env_creds`
//! - `ATHEN_HTTP_ADDR` — opt-in HTTP API for remote clients (REST + SSE,
//!   token-gated) — see `http_api`
//!
//! Setup mirrors `lib.rs`'s Tauri `setup()` hook step for step; if you add
//! a background loop there, add it here (or consciously skip it and note
//! why below). Skipped on purpose:
//! - proactive hint checker (hints are GUI cards pointing at Settings)
//! - tray icon / window focus tracking / updater plugin (no GUI)

use std::sync::Arc;

use crate::state::AppState;
use crate::ui_bridge::UiBridge;

/// Run the headless daemon. Blocks until SIGINT/SIGTERM, then runs the
/// graceful shutdown coordinator and returns.
pub fn run_headless() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("Athen starting in headless mode");

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    athen_agent::runtimes::init_portable_path();

    // Plain tokio runtime, registered as the Tauri async runtime so every
    // `tauri::async_runtime::spawn`/`block_on` in the shared code paths
    // works unchanged without a Tauri app.
    //
    // Cap worker threads: Athen's workload is overwhelmingly I/O-bound (sense
    // polling, LLM HTTP), not CPU-parallel, so `Runtime::new()`'s default of
    // one worker per core just spawns mostly-idle threads on a high-core box.
    // clamp(2, 4) leaves modest machines untouched while capping the rest.
    let worker_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 4);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());

    let ui = UiBridge::Headless;

    // The block_on calls below run on the main thread (not a runtime
    // worker), mirroring how the Tauri setup hook drives the same code.
    let mut state = tauri::async_runtime::block_on(AppState::new());

    // Sweep stale provider pins (same rationale as the desktop boot path).
    if let Some(arc_store) = state.arc_store.clone() {
        tauri::async_runtime::block_on(async move {
            match arc_store.clear_all_provider_pins().await {
                Ok(n) if n > 0 => {
                    tracing::info!(count = n, "Swept stale provider pins at startup")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Failed to sweep stale provider pins"),
            }
        });
    }

    // Reap orphaned shell_spawn'd processes from a previous run.
    if let Some(pidfile) = state.pidfile_path.clone() {
        tauri::async_runtime::block_on(async move {
            let killed = crate::spawn_pidfile::reconcile_orphans(&pidfile).await;
            if killed > 0 {
                tracing::info!(count = killed, "Reconciled orphaned spawned processes");
            }
        });
    }

    // Register an agent so tasks can be dispatched.
    let agent_id = uuid::Uuid::new_v4();
    tauri::async_runtime::block_on(async {
        state
            .coordinator
            .dispatcher()
            .register_agent(agent_id)
            .await;
    });

    // Resolve the HTTP API config before the approval router: a live
    // event bus is what makes init_approval_router construct the InApp
    // sink (remote clients answer approval questions over HTTP).
    let data_dir =
        athen_core::paths::athen_data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let http_cfg = crate::http_api::HttpApiConfig::from_env(&data_dir);
    if http_cfg.is_some() {
        UiBridge::init_event_bus();
    }

    state.init_notifier(ui.clone());
    state.init_agent_registry(ui.clone());
    state.init_approval_router(ui.clone());

    // Surface the channel situation loudly: with no GUI and no Telegram,
    // HumanConfirm tasks fail closed (the ask errors and the task sits
    // unactioned) and notifications go nowhere.
    let cfg = state.load_hydrated_config_sync();
    let telegram_ready = cfg.telegram.enabled
        && !cfg.telegram.bot_token.is_empty()
        && cfg.telegram.owner_user_id.is_some();
    if telegram_ready {
        tracing::info!("Headless user surface: Telegram (notifications + approvals + owner chat)");
    } else if http_cfg.is_some() {
        tracing::warn!(
            "Headless mode without a configured Telegram bot: approval questions reach \
             only connected HTTP clients (SSE `approval-question` events). Unattended \
             HumanConfirm tasks will stall until a client answers."
        );
    } else {
        tracing::warn!(
            "Headless mode without a configured Telegram bot: no notification or approval \
             channel exists. Anything needing human confirmation will fail closed. \
             Configure [telegram] in config.toml (+ ATHEN_TELEGRAM_BOT_TOKEN) to fix this."
        );
    }

    // The window-focus heuristic doesn't exist headless — the user is by
    // definition not looking at an app window.
    if let Some(notifier) = state.notifier.load_full() {
        notifier.set_user_present(false);
    }

    // Owner-contact migration before any monitor snapshots owner identifiers.
    if let Some(ref store) = state.contact_store {
        let store = store.clone();
        let telegram_cfg = cfg.telegram.clone();
        tauri::async_runtime::block_on(async move {
            crate::owner_migration::migrate_telegram_owner_to_contacts(&store, &telegram_cfg).await
        });
    }

    // Background loops — same order as the desktop setup hook.
    state.start_email_monitor(ui.clone());
    state.start_calendar_monitor(ui.clone());
    state.start_calendar_sync(None);
    state.start_telegram_monitor(ui.clone());
    state.start_attachment_purger();
    state.start_dispatch_loop(ui.clone());
    state.start_wakeup_scheduler(ui.clone());
    state.start_agent_run_pruner();

    // Publish the state singleton, then start the one loop that resolves
    // state through the bridge eagerly. Loops started above only touch
    // `UiBridge::app_state()` on their first event (network-latency away),
    // and the OnceLock `wait()` covers even that window.
    let state = Arc::new(state);
    UiBridge::publish_headless_state(state.clone());
    state.start_embedder_warmup(ui);

    // HTTP API for remote clients (React / React Native). Spawned after
    // the state singleton is published so request handlers never block
    // on the OnceLock.
    if let Some(cfg) = http_cfg {
        tracing::info!(addr = %cfg.addr, "HTTP API enabled (remote clients + SSE events)");
        tauri::async_runtime::spawn(async move {
            if let Err(e) = crate::http_api::serve(cfg, UiBridge::Headless).await {
                tracing::error!(error = %e, "HTTP API server exited");
            }
        });
    }

    tracing::info!("Athen headless daemon running (SIGINT/SIGTERM to stop)");

    // Park until a termination signal, then drain.
    rt.block_on(async {
        wait_for_shutdown_signal().await;
        tracing::info!("Shutdown signal received; draining");
        state.shutdown_all().await;
    });

    tracing::info!("Athen headless daemon stopped");
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "SIGTERM handler unavailable; ctrl-c only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
