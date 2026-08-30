//! P-arch/TASK-504/TASK-605：generation-aware 只读会话 RPC/SSE 唯一线上 DTO。

use crate::{ErrorEnvelope, SequencedEvent, SessionId};
use serde::{Deserialize, Serialize};

/// timeline 查询参数；cursor 是 turn 下标，limit 必须大于零。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTimelineQuery {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    pub limit: u32,
}

/// timeline 的稳定终态标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnStatus {
    Completed,
    Aborted,
    Active,
}

/// 一个 turn 的只读投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurnSummary {
    pub turn_id: u64,
    pub start_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seq: Option<u64>,
    pub status: SessionTurnStatus,
}

/// timeline 页；客户端只缓存视图，JSONL 事件流仍是唯一真相源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTimelinePage {
    pub session_id: SessionId,
    #[serde(default)]
    pub connection_generation: u64,
    pub turns: Vec<SessionTurnSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

/// SSE 补洞请求。last_seq 表示客户端已完整接收的最后一个序号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventQuery {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
}

/// 一条 SSE data 记录；`record.seq` 同时写入 SSE `id` 字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventFrame {
    pub session_id: SessionId,
    #[serde(default)]
    pub connection_generation: u64,
    pub record: SequencedEvent,
}

/// 只读服务能力协商；客户端必须把 generation 带回后续请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRpcCapabilities {
    pub connection_generation: u64,
    pub read_only: bool,
    pub timeline: bool,
    pub event_stream: bool,
    pub last_event_id: bool,
    pub follow_before_page: bool,
    pub retry_business_errors: bool,
}

/// 所有 HTTP 失败的统一线上信封。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcErrorResponse {
    pub error: ErrorEnvelope,
    /// 业务错误永不建议自动重试；传输中断没有此信封，由客户端续接。
    #[serde(default)]
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, Event};

    #[test]
    fn timeline_dtos_roundtrip_with_stable_status_and_cursor_fields() {
        let page = SessionTimelinePage {
            session_id: "demo".into(),
            connection_generation: 4,
            turns: vec![SessionTurnSummary {
                turn_id: 7,
                start_seq: 3,
                end_seq: Some(8),
                status: SessionTurnStatus::Completed,
            }],
            next_cursor: Some(1),
        };
        let json = serde_json::to_string(&page).unwrap();
        assert!(json.contains(r#""status":"completed""#));
        assert_eq!(
            serde_json::from_str::<SessionTimelinePage>(&json).unwrap(),
            page
        );
    }

    #[test]
    fn event_frame_and_resume_query_roundtrip_without_cursor_ambiguity() {
        let query = SessionEventQuery {
            session_id: "demo".into(),
            connection_generation: Some(4),
            last_seq: Some(41),
        };
        assert_eq!(
            serde_json::from_str::<SessionEventQuery>(&serde_json::to_string(&query).unwrap())
                .unwrap(),
            query
        );
        let frame = SessionEventFrame {
            session_id: "demo".into(),
            connection_generation: 4,
            record: SequencedEvent {
                seq: 42,
                event: Event::TurnCompleted { turn_id: 9 },
            },
        };
        assert_eq!(
            serde_json::from_str::<SessionEventFrame>(&serde_json::to_string(&frame).unwrap())
                .unwrap(),
            frame
        );
    }

    #[test]
    fn rpc_errors_keep_new_stable_codes() {
        for (code, encoded) in [
            (ErrorCode::SessionNotFound, "session_not_found"),
            (ErrorCode::CursorInvalid, "cursor_invalid"),
        ] {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!(r#""{encoded}""#)
            );
            let response = RpcErrorResponse {
                error: ErrorEnvelope::new(code, "human-readable only"),
                retryable: false,
            };
            assert_eq!(
                serde_json::from_str::<RpcErrorResponse>(
                    &serde_json::to_string(&response).unwrap()
                )
                .unwrap(),
                response
            );
        }
    }

    #[test]
    fn optional_cursors_are_omitted_but_decode_to_none() {
        let query = SessionTimelineQuery {
            session_id: "demo".into(),
            connection_generation: None,
            cursor: None,
            limit: 20,
        };
        let json = serde_json::to_string(&query).unwrap();
        assert!(!json.contains("cursor"));
        assert_eq!(
            serde_json::from_str::<SessionTimelineQuery>(&json).unwrap(),
            query
        );
    }

    #[test]
    fn capabilities_are_read_only_and_generation_aware() {
        let capabilities = SessionRpcCapabilities {
            connection_generation: 9,
            read_only: true,
            timeline: true,
            event_stream: true,
            last_event_id: true,
            follow_before_page: true,
            retry_business_errors: false,
        };
        assert_eq!(
            serde_json::from_str::<SessionRpcCapabilities>(
                &serde_json::to_string(&capabilities).unwrap()
            )
            .unwrap(),
            capabilities
        );
    }

    #[test]
    fn pre_task_605_responses_decode_with_unknown_generation() {
        let page = r#"{"session_id":"demo","turns":[]}"#;
        assert_eq!(
            serde_json::from_str::<SessionTimelinePage>(page)
                .unwrap()
                .connection_generation,
            0
        );
        let frame = r#"{"session_id":"demo","record":{"seq":0,"event":{"type":"turn_started","turn_id":1}}}"#;
        assert_eq!(
            serde_json::from_str::<SessionEventFrame>(frame)
                .unwrap()
                .connection_generation,
            0
        );
    }
}
