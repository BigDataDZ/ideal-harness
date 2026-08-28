//! P2/TASK-205：按运行时能力动态广告工具提权出口。

use crate::ToolSpec;
use protocol::{ErrorCode, ErrorEnvelope};

/// 上层组合沙箱模式与受限执行后端状态后，传给工具层的提权能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationAvailability {
    /// 全开放模式、后端未挂载或审批链路不可用；不得向模型展示提权出口。
    Unavailable,
    /// 当前处于受限模式，且受限执行与审批后端均已挂载。
    RestrictedBackendMounted,
}

pub(crate) fn advertised_parameters_schema(
    spec: &ToolSpec,
    availability: EscalationAvailability,
) -> Result<serde_json::Value, ErrorEnvelope> {
    let mut schema = spec.parameters_schema.clone();
    if !spec.escalation_capable || availability != EscalationAvailability::RestrictedBackendMounted
    {
        return Ok(schema);
    }

    let root = schema.as_object_mut().ok_or_else(|| {
        ErrorEnvelope::new(
            ErrorCode::Internal,
            "tool parameters schema must be an object before escalation advertisement",
        )
    })?;
    let properties = root
        .entry("properties")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                "tool schema properties must be an object before escalation advertisement",
            )
        })?;

    if properties.contains_key("sandbox_permissions") || properties.contains_key("justification") {
        return Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            "tool schema collides with reserved escalation properties",
        ));
    }

    properties.insert(
        "sandbox_permissions".into(),
        serde_json::json!({
            "type": "string",
            "enum": ["workspace-write", "danger-full-access"],
            "description": "Request a wider sandbox mode; must be paired with justification"
        }),
    );
    properties.insert(
        "justification".into(),
        serde_json::json!({
            "type": "string",
            "description": "Explain why wider sandbox permissions are required"
        }),
    );
    Ok(schema)
}
