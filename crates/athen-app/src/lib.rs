//! Athen desktop application -- Tauri composition root.
//!
//! This crate wires all Athen components together and exposes them
//! to the frontend through Tauri IPC commands.

pub(crate) mod agent_registry;
pub(crate) mod app_tools;
pub(crate) mod approval;
pub(crate) mod athen_docs;
pub(crate) mod attachment_purger;
pub(crate) mod bundle_settings;
pub(crate) mod calendar_sources;
mod commands;
pub(crate) mod compaction;
mod contacts;
pub(crate) mod delegation;
pub(crate) mod email_autodetect;
pub(crate) mod email_errors;
pub(crate) mod email_gate;
pub(crate) mod email_test;
pub(crate) mod endpoints_render;
pub(crate) mod file_gate;
pub(crate) mod github_identity;
pub(crate) mod http_presets;
pub(crate) mod http_rate_limiter;
pub(crate) mod identity_render;
pub(crate) mod mission_render;
pub(crate) mod notifier;
pub(crate) mod owner_migration;
pub(crate) mod process;
pub(crate) mod sense_router;
mod settings;
mod settings_calendar;
pub(crate) mod skills_render;
pub(crate) mod spawn_pidfile;
pub(crate) mod state;
pub(crate) mod telegram_progress;
pub(crate) mod vault_creds;
mod wakeup_commands;
pub(crate) mod wakeup_registry;
pub(crate) mod wakeup_sink;
pub(crate) mod wakeup_tool;

use state::AppState;

/// Build and run the Tauri application.
pub fn run() {
    // Initialize tracing with RUST_LOG env filter (defaults to info).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Install the rustls crypto provider before anything else uses TLS.
    // Both reqwest (for LLM calls) and rustls-connector (for IMAP) need this.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // If the wizard has previously installed portable Python / Node into
    // <athen_data_dir>/toolbox/runtimes/, prepend their bin dirs to PATH
    // BEFORE anything probes for runtimes or builds shell envs. Done at
    // process scope so every later Command::new("python") / "node" /
    // "pip" / "npm" resolves to the portable copy without per-call
    // plumbing.
    athen_agent::runtimes::init_portable_path();
    let app = tauri::Builder::default()
        // Single-instance must be registered first: when a second launch
        // happens (app runner, desktop file, CLI), the plugin's lock
        // bounces it and runs this callback in the original process so
        // we can raise the existing window instead of starting a new one.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::send_message,
            commands::get_status,
            commands::approve_task,
            commands::submit_approval,
            commands::cancel_task,
            commands::cancel_agent,
            commands::queue_user_input,
            commands::new_arc,
            commands::get_arc_history,
            commands::get_arc_entries,
            commands::compact_arc,
            commands::list_arcs,
            commands::switch_arc,
            commands::rename_arc,
            commands::delete_arc,
            commands::get_current_arc,
            commands::branch_arc,
            commands::merge_arcs,
            commands::get_timeline_data,
            settings::get_settings,
            settings::save_provider,
            settings::delete_provider,
            settings::test_provider,
            settings::save_settings,
            settings::set_active_provider,
            bundle_settings::list_bundles,
            bundle_settings::create_bundle,
            bundle_settings::update_bundle,
            bundle_settings::delete_bundle,
            bundle_settings::set_active_bundle,
            bundle_settings::duplicate_bundle,
            settings::is_first_launch,
            settings::complete_onboarding,
            settings::detect_device_capabilities,
            settings::list_provider_catalog,
            settings::list_curated_models,
            settings::list_model_families,
            settings::save_email_settings,
            settings::test_email_connection,
            settings::save_smtp_settings,
            settings::test_smtp_connection,
            settings::save_telegram_settings,
            settings::test_telegram_connection,
            settings::get_github_identities,
            settings::save_github_identity,
            settings::test_github_identity,
            settings::save_web_search_settings,
            settings::test_web_search_provider,
            settings::get_attachment_policy_settings,
            settings::save_attachment_policy_settings,
            commands::list_calendar_events,
            commands::create_calendar_event,
            commands::update_calendar_event,
            commands::delete_calendar_event,
            settings_calendar::list_calendar_sources,
            settings_calendar::add_caldav_source,
            settings_calendar::delete_calendar_source,
            settings_calendar::set_calendar_source_enabled,
            settings_calendar::set_calendar_source_selected_calendars,
            settings_calendar::test_calendar_source_connection,
            settings_calendar::list_remote_calendars,
            settings_calendar::sync_calendar_source_now,
            settings_calendar::sync_all_calendar_sources_now,
            settings_calendar::list_writable_calendars,
            settings::get_calendar_prompt,
            settings::save_calendar_prompt,
            settings::get_agent_default_calendar,
            settings::save_agent_default_calendar,
            contacts::list_contacts,
            contacts::get_contact,
            contacts::set_contact_trust,
            contacts::block_contact,
            contacts::unblock_contact,
            contacts::delete_contact,
            contacts::create_contact,
            contacts::update_contact,
            contacts::get_owner_contact,
            contacts::save_owner_contact,
            contacts::clear_owner_contact,
            commands::mark_notification_seen,
            commands::list_notifications,
            commands::mark_notification_read,
            commands::mark_all_notifications_read,
            commands::delete_notification,
            commands::delete_read_notifications,
            settings::get_notification_settings,
            settings::save_notification_settings,
            settings::save_embedding_settings,
            settings::test_embedding_provider,
            commands::list_memories,
            commands::update_memory,
            commands::delete_memory,
            commands::list_entities,
            commands::list_relations,
            commands::update_entity,
            commands::delete_entity,
            commands::delete_relation,
            commands::list_mcp_catalog,
            commands::enable_mcp,
            commands::disable_mcp,
            commands::mcp_list_custom,
            commands::mcp_list_enabled,
            commands::mcp_add_custom,
            commands::mcp_remove_custom,
            commands::mcp_set_enabled,
            commands::mcp_test_spawn,
            commands::mcp_list_tools_for,
            commands::mcp_set_risks,
            commands::list_pending_grants,
            commands::resolve_pending_grant,
            commands::list_arc_grants,
            commands::list_global_grants,
            commands::add_global_grant,
            commands::revoke_arc_grant,
            commands::revoke_global_grant,
            commands::list_agent_profiles,
            commands::set_arc_profile,
            commands::set_arc_reasoning_effort,
            commands::set_arc_tier,
            commands::create_agent_profile,
            commands::update_agent_profile,
            commands::delete_agent_profile,
            commands::restore_agent_profile,
            commands::estimate_profile_tokens,
            commands::estimate_identity_total,
            commands::list_identity_categories,
            commands::upsert_identity_category,
            commands::delete_identity_category,
            commands::list_identity_entries,
            commands::upsert_identity_entry,
            commands::delete_identity_entry,
            commands::dismiss_identity_entry,
            commands::list_skills,
            commands::get_skill,
            commands::upsert_skill,
            commands::delete_skill,
            commands::sync_skills,
            commands::list_attachments_for_event,
            commands::check_for_update,
            commands::install_update,
            commands::open_external_url,
            commands::list_toolbox_packages,
            commands::clear_toolbox,
            commands::get_runtime_status,
            commands::install_runtime,
            wakeup_commands::create_wakeup,
            wakeup_commands::update_wakeup,
            wakeup_commands::list_wakeups,
            wakeup_commands::delete_wakeup,
            wakeup_commands::set_wakeup_enabled,
            wakeup_commands::list_available_tools,
            commands::vault_smoke_test,
            commands::list_http_endpoints,
            commands::upsert_http_endpoint,
            commands::delete_http_endpoint,
            commands::set_http_endpoint_enabled,
            commands::test_http_endpoint,
            commands::list_http_endpoint_presets,
            commands::list_active_agents,
            commands::list_recent_agent_runs,
            commands::list_arc_snapshots,
            commands::revert_snapshot,
            commands::rewind_changes,
            commands::email_detect,
            commands::email_test_connection,
            commands::email_translate_error,
        ])
        .setup(|app| {
            use tauri::Manager;

            // Build the application state asynchronously (loads config, opens database).
            let mut state = tauri::async_runtime::block_on(AppState::new());

            // Reap orphaned `shell_spawn`'d processes from a previous run
            // BEFORE any monitor can start firing new wake-ups (which can
            // themselves shell_spawn). The pidfile is the only durable
            // record of these — the in-memory map is, by definition, empty
            // in a fresh process. Acceptable false-positive: PID reuse
            // means we might briefly nuke an unrelated process; better
            // that than a leaked watcher pinning the bundled nu.exe.
            if let Some(pidfile) = state.pidfile_path.clone() {
                tauri::async_runtime::block_on(async move {
                    let killed = crate::spawn_pidfile::reconcile_orphans(&pidfile).await;
                    if killed > 0 {
                        tracing::info!(
                            count = killed,
                            "Reconciled orphaned spawned processes from previous run"
                        );
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

            // Initialize the notification orchestrator (needs AppHandle for InApp channel).
            state.init_notifier(app.handle().clone());

            // Initialize the live agent registry. Needs AppHandle for the
            // `agents-changed` event the FE listens to. Wired here (between
            // notifier and approval_router) so every executor entry point
            // sees a Some(registry) on the AppState by the time it runs.
            state.init_agent_registry(app.handle().clone());

            // Initialize the approval router (InApp + Telegram sinks). Must
            // come before start_telegram_monitor so the poll loop can pick
            // up the Telegram sink for callback resolution.
            state.init_approval_router(app.handle().clone());

            // Migrate the legacy TelegramConfig::owner_user_id into the
            // unified contact store BEFORE any sense monitor starts —
            // sense monitors snapshot the owner identifier set per
            // poll, and we want the first poll to already see the
            // migrated contact instead of falling back to the legacy
            // config path. Idempotent across restarts.
            if let Some(ref store) = state.contact_store {
                let cfg = crate::settings::load_main_config_public();
                let store = store.clone();
                tauri::async_runtime::block_on(async move {
                    crate::owner_migration::migrate_telegram_owner_to_contacts(
                        &store,
                        &cfg.telegram,
                    )
                    .await
                });
            }

            // Start background monitor tasks before managing state.
            state.start_email_monitor(app.handle().clone());
            state.start_calendar_monitor(app.handle().clone());
            state.start_calendar_sync(Some(app.handle().clone()));
            state.start_telegram_monitor(app.handle().clone());

            // Sweep attachment bytes past the policy TTL. Cheap, runs
            // hourly, only deletes the bytes — extracted-text sidecars
            // outlive the purge so arc continuity is preserved.
            state.start_attachment_purger();

            // Start the autonomous-execution dispatch loop. Must come
            // after the agent has been registered with the coordinator's
            // dispatcher (above), otherwise dispatch_next_with_task
            // can't assign anything.
            state.start_dispatch_loop(app.handle().clone());

            // Spawn the wake-up scheduler loop. Phase 3a uses a logging
            // sink that persists a system arc entry on each fire — Phase
            // 3b will swap in a coordinator-backed sink that turns fires
            // into Tasks.
            state.start_wakeup_scheduler(app.handle().clone());

            // Sweep finalized agent_runs older than 30 days. Cheap; runs
            // once at startup and then every 6 hours.
            state.start_agent_run_pruner();

            app.manage(state);

            // Track window focus state for the notification orchestrator.
            // When the window loses focus, the orchestrator routes notifications
            // to external channels (Telegram) instead of in-app.
            // Also intercept window close: hide to tray instead of exiting,
            // so the Telegram poll loop and other background work keep
            // running. The tray menu provides a real Quit.
            let state_ref = app.state::<AppState>();
            let notifier_for_focus = state_ref.notifier.clone();
            if let Some(window) = app.get_webview_window("main") {
                let win_for_event = window.clone();
                window.on_window_event(move |event| match event {
                    tauri::WindowEvent::Focused(focused) => {
                        if let Some(ref notifier) = notifier_for_focus {
                            notifier.set_user_present(*focused);
                        }
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = win_for_event.hide();
                    }
                    _ => {}
                });
            }

            // Tray icon: left-click toggles the window, right-click menu
            // exposes Show / Quit. `Quit` is the only path that actually
            // exits the process now that close-to-tray is wired.
            use tauri::menu::{MenuBuilder, MenuItemBuilder};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let show_item = MenuItemBuilder::with_id("show", "Show Athen").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&show_item)
                .separator()
                .item(&quit_item)
                .build()?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .cloned()
                        .ok_or("missing default window icon")?,
                )
                .tooltip("Athen")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            // Toggle: hide if visible+focused, show otherwise.
                            let visible = w.is_visible().unwrap_or(false);
                            if visible {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Athen");

    // Intercept ExitRequested so we can run the async shutdown coordinator
    // before the process dies. The window-close path is already wired to
    // hide-to-tray (above) — Quit via tray menu / auto-updater restart /
    // OS signal (SIGTERM) is what funnels through here.
    //
    // The flow is: ExitRequested fires once → we `api.prevent_exit()`,
    // spawn the async `shutdown_all()`, then call `handle.exit(code)`
    // from inside the spawned future. That re-enters this callback;
    // a static `AtomicBool` guard lets the second pass through.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
            use std::sync::atomic::{AtomicBool, Ordering};
            static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
            if SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
                // Re-entry from our own handle.exit() at the end of the
                // async block — let it through.
                return;
            }
            api.prevent_exit();
            let handle = app_handle.clone();
            let exit_code = code.unwrap_or(0);
            tauri::async_runtime::spawn(async move {
                use tauri::Manager;
                let state: tauri::State<AppState> = handle.state();
                // Cap total shutdown at 5s so a wedged step can't trap
                // the user. shutdown_all has its own per-step timeouts
                // for finer-grained protection.
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(5), state.shutdown_all())
                        .await;
                handle.exit(exit_code);
            });
        }
    });
}
