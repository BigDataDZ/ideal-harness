//! TASK-205 验收：提权出口只在受限后端挂载时动态注入。

use protocol::ErrorCode;
use tools::{EscalationAvailability, ToolSpec};

fn capable_spec() -> ToolSpec {
    ToolSpec {
        name: "exec".into(),
        description: "execute a command".into(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["command"],
            "properties": {
                "command": { "type": "string" }
            }
        }),
        escalation_capable: true,
        timeout_ms: None,
    }
}

#[test]
fn restricted_backend_injects_paired_escalation_properties() {
    let schema = capable_spec()
        .advertised_parameters_schema(EscalationAvailability::RestrictedBackendMounted)
        .unwrap();
    let properties = schema["properties"].as_object().unwrap();

    assert!(properties.contains_key("sandbox_permissions"));
    assert!(properties.contains_key("justification"));
    assert_eq!(
        properties["sandbox_permissions"]["enum"],
        serde_json::json!(["workspace-write", "danger-full-access"])
    );
}

#[test]
fn unrestricted_or_unmounted_runtime_hides_escalation_properties() {
    let schema = capable_spec()
        .advertised_parameters_schema(EscalationAvailability::Unavailable)
        .unwrap();
    let properties = schema["properties"].as_object().unwrap();

    assert!(!properties.contains_key("sandbox_permissions"));
    assert!(!properties.contains_key("justification"));
}

#[test]
fn incapable_tool_never_advertises_escalation() {
    let mut spec = capable_spec();
    spec.escalation_capable = false;
    let schema = spec
        .advertised_parameters_schema(EscalationAvailability::RestrictedBackendMounted)
        .unwrap();

    assert_eq!(schema, spec.parameters_schema);
}

#[test]
fn advertisement_does_not_mutate_registered_base_schema() {
    let spec = capable_spec();
    let original = spec.parameters_schema.clone();
    let advertised = spec
        .advertised_parameters_schema(EscalationAvailability::RestrictedBackendMounted)
        .unwrap();

    assert_eq!(spec.parameters_schema, original);
    assert_ne!(advertised, original);
}

#[test]
fn malformed_or_colliding_schema_fails_closed() {
    let mut malformed = capable_spec();
    malformed.parameters_schema["properties"] = serde_json::json!([]);
    let error = malformed
        .advertised_parameters_schema(EscalationAvailability::RestrictedBackendMounted)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);

    let mut colliding = capable_spec();
    colliding.parameters_schema["properties"]["justification"] =
        serde_json::json!({ "type": "string" });
    let error = colliding
        .advertised_parameters_schema(EscalationAvailability::RestrictedBackendMounted)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
}
