//! TASK-607：工具结果进模型前的安全中间件——可检查、脱敏或拒绝。
//! 中间件对一切工具结果有裁决权；对插件来源（未受监管）的结果是强制的：
//! 中间件缺席或自身失败时 fail-closed，结果以稳定错误码回给模型并留 Event。

use protocol::{ErrorCode, ErrorEnvelope, ToolOutcome};
use tools::ToolRegistry;

/// 一次待裁决的工具结果上下文。
#[derive(Debug, Clone, Copy)]
pub struct ToolResultContext<'a> {
    pub call_id: &'a str,
    pub tool: &'a str,
    pub outcome: &'a ToolOutcome,
}

/// 中间件裁决：放行、以新结果替换（脱敏），或以稳定错误码拒绝。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultDecision {
    Allow,
    /// 用替换结果进入模型表面；事件流只记录替换后的结果（模型表面 = 事件流投影）。
    Redact(ToolOutcome),
    Reject(ErrorEnvelope),
}

/// 工具结果安全中间件。`inspect` 返回 `Err` 视为中间件自身失败，一律 fail-closed。
pub trait ToolResultMiddleware: Send + Sync {
    fn inspect(&self, context: &ToolResultContext<'_>)
        -> Result<ToolResultDecision, ErrorEnvelope>;
}

/// 结果进模型表面前的最后一道闸：
/// - 插件来源结果在中间件缺席时必须拒绝（未受监管的数据不得进模型）；
/// - 中间件在位时对一切结果有 Allow/Redact/Reject 裁决权；
/// - 中间件自身失败按 fail-closed 处理。
///
/// 所有裁决结果都会经 `ToolResultAdded` 落 Event，tool_call/result 配对不受影响。
pub fn guard_tool_result(
    middleware: Option<&dyn ToolResultMiddleware>,
    registry: &ToolRegistry,
    call_id: &str,
    tool: &str,
    outcome: ToolOutcome,
) -> ToolOutcome {
    let guarded = registry.plugin_provenance(tool).is_some();
    let Some(middleware) = middleware else {
        if guarded {
            return ToolOutcome::Failure {
                error: ErrorEnvelope::new(
                    ErrorCode::Internal,
                    format!(
                        "security result middleware is absent; refusing to expose plugin tool result: {tool}"
                    ),
                ),
            };
        }
        return outcome;
    };
    let context = ToolResultContext {
        call_id,
        tool,
        outcome: &outcome,
    };
    match middleware.inspect(&context) {
        Ok(ToolResultDecision::Allow) => outcome,
        Ok(ToolResultDecision::Redact(replacement)) => replacement,
        Ok(ToolResultDecision::Reject(error)) => ToolOutcome::Failure { error },
        Err(failure) => ToolOutcome::Failure {
            error: ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("security result middleware failed: {}", failure.message),
            ),
        },
    }
}
