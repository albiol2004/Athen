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
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::send_message,
            commands::get_status,
        ])
        .setup(|app| {
            // Register an agent now that the async runtime is available.
            use tauri::Manager;
            let state = app.state::<AppState>();
            let agent_id = uuid::Uuid::new_v4();
            // Use block_on since setup runs synchronously but we have a runtime.
            tauri::async_runtime::block_on(async {
                state.coordinator.dispatcher().register_agent(agent_id).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Athen");
}
