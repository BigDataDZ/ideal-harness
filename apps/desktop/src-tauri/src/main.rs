//! D25/TASK-902: restricted desktop entry point backed by the shared Host assembly.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[tauri::command]
fn desktop_status() -> String {
    match harness_host::HostConfig::default().validate() {
        Ok(_) => format!(
            "Rust 宿主已连接 · ideal-harness v{} · capability 默认拒绝",
            env!("CARGO_PKG_VERSION")
        ),
        Err(_) => "Rust 宿主配置不可用 · capability 默认拒绝".into(),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![desktop_status])
        .run(tauri::generate_context!())
        .expect("Tauri desktop runtime failed to start");
}

#[cfg(test)]
mod tests {
    use super::desktop_status;

    #[test]
    fn desktop_status_reports_version_and_default_deny_boundary() {
        let status = desktop_status();
        assert!(status.contains(env!("CARGO_PKG_VERSION")));
        assert!(status.contains("capability 默认拒绝"));
    }

    #[test]
    fn desktop_uses_the_shared_host_config_boundary() {
        assert!(harness_host::HostConfig::default().validate().is_ok());
    }
}
