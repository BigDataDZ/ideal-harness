//! D25/TASK-903: thin restricted Tauri entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::{
    cancel_turn, close_window, desktop_status, initialize_state, respond_approval,
    session_operation, start_turn, steer_turn, stop_turn,
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
            session_operation
        ])
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                close_window(window);
            }
        })
        .run(tauri::generate_context!())
        .expect("Tauri desktop runtime failed to start");
}
