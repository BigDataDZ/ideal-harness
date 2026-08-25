//! wire 协议：唯一契约（P-架构：protocol-first）。
//! 所有跨进程/跨客户端的数据结构只在此定义。
//! 变更协议 = 契约变更：必须同步更新版本说明与全部客户端。

use serde::{Deserialize, Serialize};

/// 会话标识。newtype 化用 String 承载，避免裸 String 混淆语义。
pub type SessionId = String;

/// 稳定错误码（P3：错误处理只允许匹配 code，禁止解析 message 文本）。
/// 新增变体是向后兼容的；删除/改义是破坏性契约变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ToolArgsInvalid,
    SandboxDenied,
    ApprovalRejected,
    ContextWindowExceeded,
    ModelStreamBroken,
    Internal,
}

/// 结构化错误信封：code 供机器路由，message 仅供人/模型阅读。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorEnvelope {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success { value: serde_json::Value },
    Failure { error: ErrorEnvelope },
}

/// 会话事件流：append-only，事件溯源的唯一载体（P5）。
/// 所有"魔法"（自动压缩/重试/审批）必须在这里留下痕迹（P7）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted { turn_id: u64 },
    UserMessage { text: String },
    AssistantMessage { text: String },
    ToolCallRequested { call_id: String, tool: String, args: serde_json::Value },
    ToolResultAdded { call_id: String, outcome: ToolOutcome },
    CompactionApplied { summary: String },
    ApprovalDecided { call_id: String, approved: bool },
    TurnCompleted { turn_id: u64 },
    TurnAborted { turn_id: u64, reason: String },
}

/// 带序号的事件记录：session 层持久化与流式补洞的最小单元。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serde_roundtrip() {
        let e = Event::ToolResultAdded {
            call_id: "c1".into(),
            outcome: ToolOutcome::Failure {
                error: ErrorEnvelope::new(ErrorCode::SandboxDenied, "[sandbox: denied]"),
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
        assert!(json.contains("tool_result_added"));
        assert!(json.contains("sandbox_denied"), "error code 必须序列化为稳定 snake_case");
    }

    #[test]
    fn error_code_is_stable_across_roundtrip() {
        for (code, tag) in [
            (ErrorCode::ToolArgsInvalid, "tool_args_invalid"),
            (ErrorCode::ContextWindowExceeded, "context_window_exceeded"),
            (ErrorCode::ApprovalRejected, "approval_rejected"),
        ] {
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{tag}\""));
        }
    }
}
