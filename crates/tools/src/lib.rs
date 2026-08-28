//! 工具注册表（P3）：schema 定义 + 参数校验 + 统一调度。
//! 错误一律以稳定 ErrorCode 回传，供模型自纠，绝不 panic。

mod advertisement;
mod schema;

use protocol::ToolOutcome;
use serde::{Deserialize, Serialize};

pub use advertisement::EscalationAvailability;
pub use schema::validate_args;

/// 工具规格：schema 即文档，schema 即校验器输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 形式的参数定义。
    ///
    /// TASK-201 支持工具协议使用的核心关键字：`type`、`enum`、`properties`、
    /// `required`、`items` 与 `additionalProperties`。描述性关键字保持透传。
    pub parameters_schema: serde_json::Value,
    /// 仅当受限沙箱后端挂载时才向模型广告提权出口（P2-4 动态 schema）。
    pub escalation_capable: bool,
}

impl ToolSpec {
    /// 生成模型可见的参数 schema；仅在受限执行后端已挂载时广告提权出口。
    pub fn advertised_parameters_schema(
        &self,
        availability: EscalationAvailability,
    ) -> Result<serde_json::Value, protocol::ErrorEnvelope> {
        advertisement::advertised_parameters_schema(self, availability)
    }
}

pub type ToolFn = dyn Fn(&serde_json::Value) -> ToolOutcome + Send + Sync;

struct RegisteredTool {
    spec: ToolSpec,
    handler: Box<ToolFn>,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    pub fn register(&mut self, spec: ToolSpec, handler: Box<ToolFn>) {
        assert!(
            self.get(&spec.name).is_none(),
            "duplicate tool name: {}",
            spec.name
        );
        self.tools.push(RegisteredTool { spec, handler });
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools
            .iter()
            .find(|t| t.spec.name == name)
            .map(|t| &t.spec)
    }

    /// 调度：先校验后执行；未知工具与参数错误都归一为 Failure 事件而非错误通道，
    /// 保证 tool_call/result 配对永不断裂（P4）。
    pub fn dispatch(&self, name: &str, args: &serde_json::Value) -> Option<ToolOutcome> {
        let t = self.tools.iter().find(|t| t.spec.name == name)?;
        if let Err(e) = validate_args(&t.spec, args) {
            return Some(ToolOutcome::Failure { error: e });
        }
        Some((t.handler)(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ErrorCode;

    fn echo_spec() -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "demo".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
            escalation_capable: false,
        }
    }

    #[test]
    fn missing_required_arg_fails_without_invoking_handler() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|_| panic!("handler must not run on invalid args")),
        );
        match reg.dispatch("echo", &serde_json::json!({})) {
            Some(ToolOutcome::Failure { error }) => {
                assert_eq!(error.code, ErrorCode::ToolArgsInvalid)
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_returns_none_for_turn_level_handling() {
        let reg = ToolRegistry::default();
        assert!(reg.dispatch("nope", &serde_json::json!({})).is_none());
    }

    #[test]
    fn valid_dispatch_reaches_handler() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|args| ToolOutcome::Success {
                value: args["text"].clone(),
            }),
        );
        match reg.dispatch("echo", &serde_json::json!({ "text": "hi" })) {
            Some(ToolOutcome::Success { value }) => assert_eq!(value, "hi"),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_registration_is_programmer_error() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|_| ToolOutcome::Success { value: ().into() }),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reg.register(
                echo_spec(),
                Box::new(|_| ToolOutcome::Success { value: ().into() }),
            )
        }));
        assert!(result.is_err(), "重复注册必须在开发期暴露");
    }
}
