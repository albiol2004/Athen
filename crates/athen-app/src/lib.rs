//! Athen desktop application -- Tauri composition root.
//!
//! This crate wires all Athen components together and exposes them
//! to the frontend through Tauri IPC commands.

mod commands;
pub(crate) mod process;
pub(crate) mod state;

use state::AppState;

/// Build and run the Tauri application.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::send_message,
            commands::get_status,
            commands::approve_task,
            commands::new_session,
            commands::get_history,
        ])
        .setup(|app| {
            use tauri::Manager;

            // Build the application state asynchronously (loads config, opens database).
            let state = tauri::async_runtime::block_on(AppState::new());

            // Register an agent so tasks can be dispatched.
            let agent_id = uuid::Uuid::new_v4();
            tauri::async_runtime::block_on(async {
                state.coordinator.dispatcher().register_agent(agent_id).await;
            });

            app.manage(state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Athen");
}
