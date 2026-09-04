//! D25/TASK-903: thin restricted Tauri entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod secret_store;
mod settings;

use commands::{
    cancel_turn, close_window, delete_api_key, desktop_status, get_provider_settings,
    initialize_state, respond_approval, save_provider_settings, session_event_frames,
    session_operation, start_turn,
    steer_turn, stop_turn, store_api_key, test_provider_connection,
};

fn main() {
    let workspace = std::env::current_dir().expect("desktop working directory must be available");
    let state = initialize_state(&workspace).expect("desktop security boundary must initialize");
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            desktop_status,
            start_turn,
            stop_turn,
            cancel_turn,
            steer_turn,
            respond_approval,
            session_operation,
            session_event_frames,
            get_provider_settings,
            save_provider_settings,
            store_api_key,
            delete_api_key,
            test_provider_connection
        ])
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                close_window(window);
            }
        })
        .run(tauri::generate_context!())
        .expect("Tauri desktop runtime failed to start");
}
