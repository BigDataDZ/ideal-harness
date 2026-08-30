//! TASK-201 验收：兼容 DSH/OpenAI 工具常用的 JSON Schema 形状。

use protocol::{ErrorCode, ToolOutcome};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tools::{validate_args, ToolRegistry, ToolSpec};

fn dsh_style_spec() -> ToolSpec {
    ToolSpec {
        name: "exec_command".into(),
        description: "Execute a command under an explicit sandbox mode".into(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["command", "sandbox_permissions", "tags", "options"],
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to execute"
                },
                "sandbox_permissions": {
                    "type": ["string", "null"],
                    "enum": ["workspace-write", "danger-full-access", null]
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["build", "test", "lint"]
                    }
                },
                "options": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["retries", "interactive"],
                    "properties": {
                        "retries": { "type": "integer" },
                        "interactive": { "type": "boolean" }
                    }
                }
            }
        }),
        escalation_capable: true,
        timeout_ms: None,
    }
}

fn valid_args() -> serde_json::Value {
    serde_json::json!({
        "command": "cargo test --workspace",
        "sandbox_permissions": null,
        "tags": ["build", "test"],
        "options": {
            "retries": 1,
            "interactive": false
        }
    })
}

#[test]
fn accepts_nested_dsh_style_schema() {
    assert!(validate_args(&dsh_style_spec(), &valid_args()).is_ok());
}

#[test]
fn rejects_type_and_enum_violations_with_stable_code() {
    let mut wrong_type = valid_args();
    wrong_type["options"]["retries"] = serde_json::json!(1.5);
    let error = validate_args(&dsh_style_spec(), &wrong_type).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);

    let mut wrong_enum = valid_args();
    wrong_enum["sandbox_permissions"] = serde_json::json!("read-only");
    let error = validate_args(&dsh_style_spec(), &wrong_enum).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);
}

#[test]
fn validates_every_array_item() {
    let mut args = valid_args();
    args["tags"] = serde_json::json!(["build", "deploy"]);
    let error = validate_args(&dsh_style_spec(), &args).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);
}

#[test]
fn additional_properties_false_applies_recursively() {
    let mut root_extra = valid_args();
    root_extra["unexpected"] = serde_json::json!(true);
    let error = validate_args(&dsh_style_spec(), &root_extra).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);

    let mut nested_extra = valid_args();
    nested_extra["options"]["timeout"] = serde_json::json!(30);
    let error = validate_args(&dsh_style_spec(), &nested_extra).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);
}

#[test]
fn additional_properties_schema_validates_unknown_values() {
    let spec = ToolSpec {
        name: "labels".into(),
        description: "string labels".into(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": { "type": "string" }
        }),
        escalation_capable: false,
        timeout_ms: None,
    };

    assert!(validate_args(&spec, &serde_json::json!({ "owner": "agent" })).is_ok());
    let error = validate_args(&spec, &serde_json::json!({ "attempts": 2 })).unwrap_err();
    assert_eq!(error.code, ErrorCode::ToolArgsInvalid);
}

#[test]
fn malformed_supported_keywords_are_internal_schema_errors() {
    let spec = ToolSpec {
        name: "broken".into(),
        description: "invalid schema".into(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "values": {
                    "type": "array",
                    "items": "not-a-schema"
                }
            }
        }),
        escalation_capable: false,
        timeout_ms: None,
    };

    let error = validate_args(&spec, &serde_json::json!({ "values": [] })).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
}

#[test]
fn invalid_nested_args_never_reach_handler() {
    let invoked = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invoked);
    let mut registry = ToolRegistry::default();
    registry.register(
        dsh_style_spec(),
        Box::new(move |_| {
            observed.store(true, Ordering::SeqCst);
            ToolOutcome::Success {
                value: serde_json::Value::Null,
            }
        }),
    );

    let mut args = valid_args();
    args["tags"] = serde_json::json!([false]);
    match registry.dispatch("exec_command", &args) {
        Some(ToolOutcome::Failure { error }) => {
            assert_eq!(error.code, ErrorCode::ToolArgsInvalid)
        }
        other => panic!("expected validation failure, got {other:?}"),
    }
    assert!(!invoked.load(Ordering::SeqCst));
}
