use protocol::ErrorCode;
use std::path::PathBuf;
use std::time::Duration;
use tools::{
    McpFailureStage, McpRegistration, McpRegistry, McpServerConfig, McpServiceRequirement,
    McpServiceStatus,
};

fn registration(source: &str, mode: &str, requirement: McpServiceRequirement) -> McpRegistration {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let config = if cfg!(windows) {
        McpServerConfig {
            source: source.into(),
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
            source: source.into(),
            program: PathBuf::from("sh"),
            args: vec![
                fixture.join("mcp_server.sh").to_string_lossy().into_owned(),
                mode.into(),
            ],
            max_output_bytes: 32,
        }
    };
    McpRegistration {
        config,
        requirement,
        discovery_grace: Duration::from_secs(5),
    }
}

#[test]
fn required_failure_rejects_registry_startup() {
    let error = McpRegistry::start(vec![registration(
        "required",
        "exit",
        McpServiceRequirement::Required,
    )])
    .err()
    .expect("required discovery failure must reject startup");
    assert_eq!(error.code, ErrorCode::Internal);
}

#[test]
fn optional_timeout_degrades_without_hiding_ready_catalog() {
    let mut optional = registration("slow", "hang_discovery", McpServiceRequirement::Optional);
    optional.discovery_grace = Duration::from_millis(150);
    let registry = McpRegistry::start(vec![
        optional,
        registration("ready", "normal", McpServiceRequirement::Required),
    ])
    .unwrap();

    let tools: Vec<_> = registry
        .tools()
        .map(|tool| format!("{}:{}", tool.source, tool.name))
        .collect();
    assert_eq!(tools, ["ready:echo", "ready:verbose"]);
    let slow = registry
        .services()
        .find(|service| service.source == "slow")
        .unwrap();
    assert_eq!(slow.status, McpServiceStatus::Degraded);
    assert_eq!(slow.failure.unwrap().stage, McpFailureStage::Discovery);
}

#[test]
fn refresh_advances_generation_and_rejects_old_handle() {
    let mut registry = McpRegistry::start(vec![registration(
        "fixture",
        "normal",
        McpServiceRequirement::Required,
    )])
    .unwrap();
    let old = registry.tool("fixture", "echo").unwrap().clone();
    assert_eq!(old.generation, 1);

    assert!(registry.refresh("fixture").unwrap());
    let current = registry.tool("fixture", "echo").unwrap().clone();
    assert_eq!(current.generation, 2);
    assert_eq!(
        registry
            .call(&old, &serde_json::json!({ "text": "stale" }))
            .unwrap_err()
            .code,
        ErrorCode::ToolArgsInvalid
    );
    assert_eq!(
        registry
            .call(&current, &serde_json::json!({ "text": "fresh" }))
            .unwrap()
            .visible_output(),
        "fresh"
    );
}

#[test]
fn one_service_call_failure_does_not_hide_other_service() {
    let mut registry = McpRegistry::start(vec![
        registration("bad", "malformed", McpServiceRequirement::Optional),
        registration("good", "normal", McpServiceRequirement::Required),
    ])
    .unwrap();
    let bad = registry.tool("bad", "echo").unwrap().clone();
    let good = registry.tool("good", "verbose").unwrap().clone();

    assert_eq!(
        registry
            .call(&bad, &serde_json::json!({ "text": "x" }))
            .unwrap_err()
            .code,
        ErrorCode::Internal
    );
    assert!(registry.tool("bad", "echo").is_none());
    assert!(registry.tool("good", "verbose").is_some());
    let result = registry.call(&good, &serde_json::json!({})).unwrap();
    assert!(result.was_truncated());
    assert_eq!(result.visible_output(), "xxxxxxxxxxxx");
}
