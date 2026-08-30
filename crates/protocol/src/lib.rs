//! wire 协议：唯一契约（P-架构：protocol-first）。
//! 所有跨进程/跨客户端的数据结构只在此定义。
//! 变更协议 = 契约变更：必须同步更新版本说明与全部客户端。

use serde::{Deserialize, Serialize};

mod session_rpc;

pub use session_rpc::{
    RpcErrorResponse, SessionEventFrame, SessionEventQuery, SessionRpcCapabilities,
    SessionTimelinePage, SessionTimelineQuery, SessionTurnStatus, SessionTurnSummary,
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
    TeamRevisionConflict,
    TeamDependencyCycle,
    /// TASK-702：工具执行超过其 deadline；底层副作用不被取消。
    ToolTimeout,
    /// TASK-702：循环防护拒绝了重复的等参工具调用。
    ToolLoopDetected,
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

/// Token 用量来源：provider 返回值优先，缺失时才允许启发式估算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageSource {
    Provider,
    Heuristic,
}

/// provider 对一次完整采样返回的 Token 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTokenUsage {
    pub total_tokens: u64,
}

/// 实际执行目标的环境事实；不能用控制端的 OS/home/path 猜测远端语义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorEnvironment {
    pub os: String,
    pub home: String,
    pub workspace: String,
    pub generation: u64,
}

/// 一次授权判断必须绑定的完整上下文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationContext {
    pub policy_epoch: u64,
    pub permission_profile_hash: String,
    pub executor: ExecutorEnvironment,
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

/// TASK-802：工具终止原因；重放与审计用稳定枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTermination {
    /// deadline 到期，进程内 handler 被协作取消，外部命令进程树被终止。
    DeadlineExceeded,
    /// 显式取消（token.cancel）且未到 deadline。
    Cancelled,
    /// 终止动作本身完成（进程/进程树已退出）。
    ProcessTreeTerminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub member_id: String,
    pub parent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMessage {
    pub message_id: String,
    pub from_member_id: String,
    pub to_member_id: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub task_id: String,
    pub owner_member_id: String,
    pub revision: u64,
    pub status: TeamTaskStatus,
    pub blocked_by: Vec<String>,
    pub write_scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamWriteScopeConflict {
    pub task_id: String,
    pub conflicting_task_id: String,
    pub scope: String,
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
    /// TASK-704：turn 运行中入队的 steer 输入；模型表面视同 User 消息，
    /// 但在工具批次未闭合时延迟出账以保住 tool_call/result 配对。
    UserInputQueued {
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
    /// TASK-602：根预算配置是可重放事实；同一会话只允许幂等重复。
    TokenBudgetConfigured {
        root_agent_id: String,
        token_budget: u64,
    },
    /// 每次成功模型采样恰好记录一次；agent_path 从根到实际消费者。
    TokenUsageRecorded {
        usage_id: String,
        agent_path: Vec<String>,
        total_tokens: u64,
        source: TokenUsageSource,
    },
    ApprovalDecided {
        call_id: String,
        approved: bool,
        /// None 仅兼容 TASK-603 之前的审批事件；新批准必须携带绑定。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorization: Option<AuthorizationContext>,
    },
    /// 审批期间权限或执行目标变化，旧判断不得继续授权。
    AuthorizationInvalidated {
        call_id: String,
        previous: AuthorizationContext,
        current: AuthorizationContext,
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
    /// TASK-802：工具执行被取消/超时/终止的结构化留痕；
    /// 紧随其后必有同 call_id 的 ToolResultAdded（失败结果），保持配对完整。
    ToolExecutionTerminated {
        call_id: String,
        termination: ToolTermination,
    },
    /// TASK-705：跨会话记忆写入；id 幂等，重放时后写覆盖同 id。
    MemoryRecorded {
        memory_id: String,
        text: String,
        tags: Vec<String>,
    },
    /// TASK-705：记忆注入系统表面的事件化事实（模型可见）。
    MemoryContextInjected {
        summary: String,
    },
    TeamMemberRegistered {
        member: TeamMember,
    },
    TeamMessageEnqueued {
        message: TeamMessage,
    },
    TeamMessageDelivered {
        message_id: String,
        to_member_id: String,
    },
    TeamTaskCreated {
        task: TeamTask,
    },
    TeamTaskUpdated {
        expected_revision: u64,
        task: TeamTask,
    },
    TeamWriteScopeConflictDetected {
        conflict: TeamWriteScopeConflict,
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
    fn tool_termination_event_roundtrip() {
        for termination in [
            ToolTermination::DeadlineExceeded,
            ToolTermination::Cancelled,
            ToolTermination::ProcessTreeTerminated,
        ] {
            let event = Event::ToolExecutionTerminated {
                call_id: "c1".into(),
                termination,
            };
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
        assert_eq!(
            serde_json::to_string(&ToolTermination::DeadlineExceeded).unwrap(),
            "\"deadline_exceeded\""
        );
    }

    #[test]
    fn memory_events_roundtrip() {
        for event in [
            Event::MemoryRecorded {
                memory_id: "mem-1".into(),
                text: "用户偏好 Rust".into(),
                tags: vec!["preference".into()],
            },
            Event::MemoryContextInjected {
                summary: "记忆: 用户偏好 Rust".into(),
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn user_input_queued_roundtrips() {
        let event = Event::UserInputQueued {
            text: "优先处理 X".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("user_input_queued"), "{json}");
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
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
    fn token_budget_and_usage_events_roundtrip() {
        let events = [
            Event::TokenBudgetConfigured {
                root_agent_id: "root".into(),
                token_budget: 1_000,
            },
            Event::TokenUsageRecorded {
                usage_id: "sample-1".into(),
                agent_path: vec!["root".into(), "child".into()],
                total_tokens: 37,
                source: TokenUsageSource::Provider,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn authorization_context_and_invalidation_roundtrip() {
        let previous = AuthorizationContext {
            policy_epoch: 7,
            permission_profile_hash: "profile-a".into(),
            executor: ExecutorEnvironment {
                os: "windows".into(),
                home: "C:/Users/test".into(),
                workspace: "D:/work".into(),
                generation: 3,
            },
        };
        let event = Event::AuthorizationInvalidated {
            call_id: "call-1".into(),
            previous: previous.clone(),
            current: AuthorizationContext {
                policy_epoch: 8,
                ..previous
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn pre_task_603_approval_event_remains_readable() {
        let old = r#"{"type":"approval_decided","call_id":"c1","approved":true}"#;
        assert_eq!(
            serde_json::from_str::<Event>(old).unwrap(),
            Event::ApprovalDecided {
                call_id: "c1".into(),
                approved: true,
                authorization: None,
            }
        );
    }

    #[test]
    fn team_coordination_contract_roundtrips_with_stable_codes() {
        let task = TeamTask {
            task_id: "task-1".into(),
            owner_member_id: "agent-a".into(),
            revision: 2,
            status: TeamTaskStatus::Blocked,
            blocked_by: vec!["task-0".into()],
            write_scopes: vec!["crates/session".into()],
        };
        for event in [
            Event::TeamMemberRegistered {
                member: TeamMember {
                    member_id: "agent-a".into(),
                    parent_id: "root".into(),
                },
            },
            Event::TeamMessageEnqueued {
                message: TeamMessage {
                    message_id: "message-1".into(),
                    from_member_id: "root".into(),
                    to_member_id: "agent-a".into(),
                    body: "continue".into(),
                },
            },
            Event::TeamMessageDelivered {
                message_id: "message-1".into(),
                to_member_id: "agent-a".into(),
            },
            Event::TeamTaskCreated { task: task.clone() },
            Event::TeamTaskUpdated {
                expected_revision: 1,
                task,
            },
            Event::TeamWriteScopeConflictDetected {
                conflict: TeamWriteScopeConflict {
                    task_id: "task-1".into(),
                    conflicting_task_id: "task-2".into(),
                    scope: "crates/session".into(),
                },
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
        assert_eq!(
            serde_json::to_string(&ErrorCode::TeamRevisionConflict).unwrap(),
            "\"team_revision_conflict\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::TeamDependencyCycle).unwrap(),
            "\"team_dependency_cycle\""
        );
    }

    #[test]
    fn error_code_is_stable_across_roundtrip() {
        for (code, tag) in [
            (ErrorCode::ToolArgsInvalid, "tool_args_invalid"),
            (ErrorCode::ContextWindowExceeded, "context_window_exceeded"),
            (ErrorCode::ApprovalRejected, "approval_rejected"),
            (ErrorCode::ToolTimeout, "tool_timeout"),
            (ErrorCode::ToolLoopDetected, "tool_loop_detected"),
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
