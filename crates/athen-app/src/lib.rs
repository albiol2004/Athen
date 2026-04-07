//! Athen desktop application -- Tauri composition root.
//!
//! This crate wires all Athen components together and exposes them
//! to the frontend through Tauri IPC commands.

pub(crate) mod app_tools;
mod commands;
mod contacts;
pub(crate) mod notifier;
pub(crate) mod process;
pub(crate) mod sense_router;
mod settings;
pub(crate) mod state;

use state::AppState;

/// Build and run the Tauri application.
pub fn run() {
    // Install the rustls crypto provider before anything else uses TLS.
    // Both reqwest (for LLM calls) and rustls-connector (for IMAP) need this.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::send_message,
            commands::get_status,
            commands::approve_task,
            commands::cancel_task,
            commands::new_arc,
            commands::get_arc_history,
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
            settings::save_email_settings,
            settings::test_email_connection,
            settings::save_telegram_settings,
            settings::test_telegram_connection,
            commands::list_calendar_events,
            commands::create_calendar_event,
            commands::update_calendar_event,
            commands::delete_calendar_event,
            contacts::list_contacts,
            contacts::get_contact,
            contacts::set_contact_trust,
            contacts::block_contact,
            contacts::unblock_contact,
            contacts::delete_contact,
            contacts::create_contact,
            contacts::update_contact,
            commands::mark_notification_seen,
            settings::get_notification_settings,
            settings::save_notification_settings,
        ])
        .setup(|app| {
            use tauri::Manager;

            // Build the application state asynchronously (loads config, opens database).
            let mut state = tauri::async_runtime::block_on(AppState::new());

            // Register an agent so tasks can be dispatched.
            let agent_id = uuid::Uuid::new_v4();
            tauri::async_runtime::block_on(async {
                state.coordinator.dispatcher().register_agent(agent_id).await;
            });

            // Initialize the notification orchestrator (needs AppHandle for InApp channel).
            state.init_notifier(app.handle().clone());

            // Start background monitor tasks before managing state.
            state.start_email_monitor(app.handle().clone());
            state.start_calendar_monitor(app.handle().clone());
            state.start_telegram_monitor(app.handle().clone());

            app.manage(state);

            // Track window focus state for the notification orchestrator.
            // When the window loses focus, the orchestrator routes notifications
            // to external channels (Telegram) instead of in-app.
            let state_ref = app.state::<AppState>();
            let notifier_for_focus = state_ref.notifier.clone();
            if let Some(window) = app.get_webview_window("main") {
                window.on_window_event(move |event| {
                    if let Some(ref notifier) = notifier_for_focus {
                        if let tauri::WindowEvent::Focused(focused) = event {
                            notifier.set_user_present(*focused);
                        }
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Athen");
}
