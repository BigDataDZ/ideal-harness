//! TASK-202 验收：命令在真实 restricted-token 子进程中执行。

use sandbox_exec::{CommandSpec, PlatformRestrictedBackend, RestrictedProcessPool};

#[cfg(windows)]
#[test]
fn external_command_runs_outside_main_process_with_restricted_token() {
    let pool = RestrictedProcessPool::new(PlatformRestrictedBackend);
    let command = CommandSpec::new("cmd.exe")
        .arg("/D")
        .arg("/C")
        .arg("echo sandbox-child");
    let output = pool.execute(&command).unwrap();

    assert_ne!(output.process_id, std::process::id());
    assert_eq!(output.exit_code, 0);
    assert!(output.restricted);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "sandbox-child"
    );
    assert!(output.stderr.is_empty());
}

#[cfg(windows)]
#[test]
fn missing_executable_fails_closed_without_fallback() {
    let pool = RestrictedProcessPool::new(PlatformRestrictedBackend);
    let command = CommandSpec::new("definitely-missing-ideal-harness-command.exe");
    assert!(pool.execute(&command).is_err());
}

#[test]
fn command_builder_preserves_requested_working_directory() {
    let command = CommandSpec::new("program")
        .arg("one")
        .current_dir("workspace");
    assert_eq!(command.args, ["one"]);
    assert_eq!(
        command.current_dir.unwrap(),
        std::path::PathBuf::from("workspace")
    );
}
