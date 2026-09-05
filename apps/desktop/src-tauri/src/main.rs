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

/// TASK-909 修复：workspace 必须是**稳定的按用户固定目录**——
/// 此前用进程 CWD，从开始菜单启动时 CWD 随机，导致设置与会话“重启即消失”。
/// 优先级：IDEAL_HARNESS_DESKTOP_WORKSPACE 环境变量 > %USERPROFILE%\.ideal-harness。
fn resolve_workspace() -> std::path::PathBuf {
    if let Ok(override_dir) = std::env::var("IDEAL_HARNESS_DESKTOP_WORKSPACE") {
        if !override_dir.trim().is_empty() {
            return std::path::PathBuf::from(override_dir);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("desktop home directory must be available");
    std::path::PathBuf::from(home).join(".ideal-harness")
}

fn main() {
    let workspace = resolve_workspace();
    std::fs::create_dir_all(&workspace)
        .expect("desktop workspace directory must be creatable");
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
