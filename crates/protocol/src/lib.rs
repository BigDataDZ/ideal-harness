//! wire 协议：唯一契约（P-架构：protocol-first）。
//! 所有跨进程/跨客户端的数据结构只在此定义。
//! 变更协议 = 契约变更：必须同步更新版本说明与全部客户端。

use serde::{Deserialize, Serialize};

mod session_rpc;

pub use session_rpc::{
    RpcErrorResponse, SessionEventFrame, SessionEventQuery, SessionTimelinePage,
    SessionTimelineQuery, SessionTurnStatus, SessionTurnSummary,
};

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
    SubagentCancelled,
    SessionNotFound,
    CursorInvalid,
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
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success { value: serde_json::Value },
    Failure { error: ErrorEnvelope },
}

/// 一次模型响应中声明的工具调用。`arguments` 保留 provider 返回的原始 JSON 文本，
/// 使 resume 后发送给模型的消息与在线路径保持一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 从事件流派生的唯一模型可见消息；客户端和 provider 适配层不得各建第二套投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelSurfaceMessage {
    SystemSummary {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    AssistantToolCalls {
        request_id: String,
        calls: Vec<ModelToolCall>,
    },
    ToolResult {
        call_id: String,
        outcome: ToolOutcome,
    },
}

/// 模型表面消息及其来源事件；压缩用来源集合证明 replace-prefix 没有越界。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSurfaceEntry {
    pub message: ModelSurfaceMessage,
    pub source_event_seqs: Vec<u64>,
}

/// 子代理的闭合终态；Started 必须恰好对应一个 Stopped。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// 子代理报告的父级投递方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentReportDelivery {
    /// 在下一个 turn 边界进入父 inbox。
    NextStep,
    /// 只留事件，不唤醒父 inbox。
    Quiet,
}

/// 会话事件流：append-only，事件溯源的唯一载体（P5）。
/// 所有"魔法"（自动压缩/重试/审批）必须在这里留下痕迹（P7）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted {
        turn_id: u64,
    },
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    /// 流式采样的增量输出块（P1/TASK-101）：一次采样中的增量文本片段。
    ModelChunkReceived {
        call_id: String,
        delta_text: String,
    },
    ToolCallRequested {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    ToolResultAdded {
        call_id: String,
        outcome: ToolOutcome,
    },
    /// TASK-601：仅描述真正发送给模型的 assistant tool_calls 批次。
    /// `ToolCallRequested` 继续承担逐调用审计，二者职责不得混用。
    ModelToolCallsRequested {
        request_id: String,
        calls: Vec<ModelToolCall>,
    },
    CompactionApplied {
        summary: String,
        /// 新事件必须填写；None 仅用于兼容 TASK-601 之前的 JSONL。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_messages: Option<u64>,
        /// 被替换表面消息对应的事件序号，按首次出现顺序去重。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        source_event_seqs: Vec<u64>,
    },
    ApprovalDecided {
        call_id: String,
        approved: bool,
    },
    /// P2/TASK-203：白名单代理拒绝外连时的稳定审计事件。
    NetworkAccessDenied {
        host: String,
        port: u16,
        reason: String,
    },
    /// TASK-410：子代理启动与 parent/child lineage 的唯一事实记录。
    SubagentStarted {
        task_id: String,
        parent_id: String,
        child_id: String,
    },
    /// 父级取消请求；必须由 cancelled 的 SubagentStopped 收口。
    SubagentCancellationRequested {
        task_id: String,
        child_id: String,
        reason: String,
    },
    /// 成功报告的投递审计；quiet/next_step 均必须留痕。
    SubagentReportDelivered {
        task_id: String,
        child_id: String,
        delivery: SubagentReportDelivery,
        text: String,
    },
    SubagentStopped {
        task_id: String,
        child_id: String,
        outcome: SubagentOutcome,
    },
    TurnCompleted {
        turn_id: u64,
    },
    TurnAborted {
        turn_id: u64,
        reason: String,
    },
}

/// 一次模型调用的规格（P1/TASK-101）。
/// 边界说明：认证字段（API key 等）属于 provider 层，
/// 刻意不进协议——见 ROADMAP TASK-101「明确不做」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCallSpec {
    /// 模型名，如 "deepseek-chat"。
    pub model: String,
    /// API base URL，如 "https://api.deepseek.com/v1"。
    pub base_url: String,
    /// None 表示使用 provider 默认温度；None 不写入线上格式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
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
        assert!(
            json.contains("sandbox_denied"),
            "error code 必须序列化为稳定 snake_case"
        );
    }

    #[test]
    fn subagent_lifecycle_events_roundtrip() {
        let events = [
            Event::SubagentStarted {
                task_id: "task-1".into(),
                parent_id: "root".into(),
                child_id: "child-1".into(),
            },
            Event::SubagentReportDelivered {
                task_id: "task-1".into(),
                child_id: "child-1".into(),
                delivery: SubagentReportDelivery::NextStep,
                text: "done".into(),
            },
            Event::SubagentStopped {
                task_id: "task-1".into(),
                child_id: "child-1".into(),
                outcome: SubagentOutcome::Succeeded,
            },
        ];
        for event in events {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn pre_task_410_event_json_remains_readable() {
        let old = r#"{"type":"turn_aborted","turn_id":7,"reason":"old session"}"#;
        assert_eq!(
            serde_json::from_str::<Event>(old).unwrap(),
            Event::TurnAborted {
                turn_id: 7,
                reason: "old session".into(),
            }
        );
    }

    #[test]
    fn pre_task_601_compaction_remains_readable() {
        let old = r#"{"type":"compaction_applied","summary":"legacy"}"#;
        assert_eq!(
            serde_json::from_str::<Event>(old).unwrap(),
            Event::CompactionApplied {
                summary: "legacy".into(),
                compacted_messages: None,
                source_event_seqs: vec![],
            }
        );
    }

    #[test]
    fn model_surface_contract_roundtrips() {
        let event = Event::ModelToolCallsRequested {
            request_id: "turn-1-round-0".into(),
            calls: vec![ModelToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"rust"}"#.into(),
            }],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
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

    #[test]
    fn model_chunk_received_roundtrip_with_snake_case_tag() {
        let e = Event::ModelChunkReceived {
            call_id: "c42".into(),
            delta_text: "你好".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            json.contains("\"type\":\"model_chunk_received\""),
            "tag 必须是稳定 snake_case: {json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn network_access_denied_roundtrip_with_stable_fields() {
        let event = Event::NetworkAccessDenied {
            host: "untrusted.example".into(),
            port: 443,
            reason: "host_not_allowlisted".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"network_access_denied","host":"untrusted.example","port":443,"reason":"host_not_allowlisted"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn model_call_spec_roundtrip_and_optional_temperature_omission() {
        let full = ModelCallSpec {
            model: "deepseek-chat".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            temperature: Some(0.7),
        };
        let j = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<ModelCallSpec>(&j).unwrap(), full);

        // temperature=None 必须从线上格式省略，读取端缺省还原为 None
        let minimal = ModelCallSpec {
            model: "m".into(),
            base_url: "u".into(),
            temperature: None,
        };
        let jm = serde_json::to_string(&minimal).unwrap();
        assert!(
            !jm.contains("temperature"),
            "None 温度不应出现于线上格式: {jm}"
        );
        assert_eq!(serde_json::from_str::<ModelCallSpec>(&jm).unwrap(), minimal);
    }

    #[test]
    fn legacy_v010_jsonl_lines_still_replay() {
        // v0.1.0 真实落盘的行（不含新变体）。向后兼容铁律：旧会话文件必须原样可读。
        let legacy_lines = [
            r#"{"seq":0,"event":{"type":"turn_started","turn_id":0}}"#,
            r#"{"seq":1,"event":{"type":"user_message","text":"你好"}}"#,
            r#"{"seq":2,"event":{"type":"assistant_message","text":"echo: 收到"}}"#,
            r#"{"seq":3,"event":{"type":"tool_result_added","call_id":"c1","outcome":{"failure":{"error":{"code":"sandbox_denied","message":"[sandbox: denied]"}}}}}"#,
        ];
        for (idx, line) in legacy_lines.iter().enumerate() {
            let se: SequencedEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("旧版本行必须仍可重放: {line} ({e})"));
            assert_eq!(se.seq, idx as u64);
        }
    }
}
