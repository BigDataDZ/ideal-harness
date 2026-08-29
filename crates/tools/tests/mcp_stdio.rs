use protocol::ErrorCode;
use std::path::PathBuf;
use tools::{McpClient, McpServerConfig};

fn config(mode: &str) -> McpServerConfig {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    if cfg!(windows) {
        McpServerConfig {
            source: "fixture".into(),
            program: PathBuf::from("powershell.exe"),
            args: vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                fixture
                    .join("mcp_server.ps1")
                    .to_string_lossy()
                    .into_owned(),
                mode.into(),
            ],
            max_output_bytes: 32,
        }
    } else {
        McpServerConfig {
            source: "fixture".into(),
            program: PathBuf::from("sh"),
            args: vec![
                fixture.join("mcp_server.sh").to_string_lossy().into_owned(),
                mode.into(),
            ],
            max_output_bytes: 32,
        }
    }
}

#[test]
fn fixture_server_initializes_discovers_and_calls_tools() {
    let mut client = McpClient::connect(config("normal")).unwrap();
    let tools: Vec<_> = client.tools().map(|tool| tool.name.as_str()).collect();
    assert_eq!(tools, vec!["echo", "verbose"]);
    let result = client
        .call("echo", &serde_json::json!({ "text": "hello" }))
        .unwrap();
    assert_eq!(result.source(), "fixture");
    assert_eq!(result.tool(), "echo");
    assert_eq!(result.visible_output(), "hello");
    assert!(!result.was_truncated());
}

#[test]
fn per_tool_output_limit_preserves_full_result_for_spill() {
    let mut client = McpClient::connect(config("normal")).unwrap();
    let result = client.call("verbose", &serde_json::json!({})).unwrap();
    assert_eq!(result.output_limit_bytes(), 12);
    assert_eq!(result.visible_output(), "xxxxxxxxxxxx");
    assert_eq!(result.full_output().len(), 64);
    assert!(result.was_truncated());
}

#[test]
fn malformed_protocol_and_child_exit_fail_closed() {
    let mut malformed = McpClient::connect(config("malformed")).unwrap();
    assert_eq!(
        malformed
            .call("echo", &serde_json::json!({ "text": "x" }))
            .unwrap_err()
            .code,
        ErrorCode::Internal
    );
    let exit_error = McpClient::connect(config("exit"))
        .err()
        .expect("exited fixture must fail connection");
    assert_eq!(exit_error.code, ErrorCode::Internal);
}

#[test]
fn invalid_arguments_are_rejected_before_server_call() {
    let mut client = McpClient::connect(config("normal")).unwrap();
    assert_eq!(
        client
            .call("echo", &serde_json::json!({}))
            .unwrap_err()
            .code,
        ErrorCode::ToolArgsInvalid
    );
}
