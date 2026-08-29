//! P-arch/TASK-504：只读会话 RPC 与 SSE 的唯一线上 DTO。

use crate::{ErrorEnvelope, SequencedEvent, SessionId};
use serde::{Deserialize, Serialize};

/// timeline 查询参数；cursor 是 turn 下标，limit 必须大于零。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTimelineQuery {
    pub session_id: SessionId,
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
    pub turns: Vec<SessionTurnSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

/// SSE 补洞请求。last_seq 表示客户端已完整接收的最后一个序号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventQuery {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
}

/// 一条 SSE data 记录；`record.seq` 同时写入 SSE `id` 字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventFrame {
    pub session_id: SessionId,
    pub record: SequencedEvent,
}

/// 所有 HTTP 失败的统一线上信封。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcErrorResponse {
    pub error: ErrorEnvelope,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, Event};

    #[test]
    fn timeline_dtos_roundtrip_with_stable_status_and_cursor_fields() {
        let page = SessionTimelinePage {
            session_id: "demo".into(),
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
            last_seq: Some(41),
        };
        assert_eq!(
            serde_json::from_str::<SessionEventQuery>(&serde_json::to_string(&query).unwrap())
                .unwrap(),
            query
        );
        let frame = SessionEventFrame {
            session_id: "demo".into(),
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
}
