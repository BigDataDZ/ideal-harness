//! TASK-704 验收：turn 内 steer——排队输入在轮边界按序可见、不拆配对、
//! 模型表面投影在工具批次未闭合时延迟出账、跨 turn 残留可被下一 turn 吸收。

use agent_loop::AgentLoop;
use model_provider::{ChatMessage, ChatModel, ChatReply, ToolCallRequest};
use protocol::{
    ErrorCode, ErrorEnvelope, Event, ModelCallSpec, ModelToolCall, SequencedEvent, ToolOutcome,
};
use session::{project_model_surface, replay, JsonlSession};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tools::ToolRegistry;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-steer-{}-{name}", std::process::id()))
}

fn spec() -> ModelCallSpec {
    ModelCallSpec {
        model: "mock-model".into(),
        base_url: "http://localhost".into(),
        temperature: None,
    }
}

/// 捕获每次采样所见消息；按预置脚本回复（None 表示最终文本答复）。
struct Capturing {
    script: Mutex<Vec<Option<String>>>,
    seen: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ChatModel for Capturing {
    fn stream_chat(
        &self,
        _: &ModelCallSpec,
        msgs: &[ChatMessage],
        _: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope> {
        self.seen.lock().unwrap().push(msgs.to_vec());
        let mut queue = self.script.lock().unwrap();
        match queue.remove(0) {
            Some(call_id) => Ok(ChatReply {
                text: "让我查一下".into(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: call_id,
                    name: "echo".into(),
                    arguments: r#"{"text":"hi"}"#.into(),
                }],
                usage: None,
            }),
            None => Ok(ChatReply {
                text: "完成".into(),
                finish_reason: Some("stop".into()),
                tool_calls: vec![],
                usage: None,
            }),
        }
    }
}

fn echo_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(
        tools::ToolSpec {
            name: "echo".into(),
            description: "demo".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(|args| ToolOutcome::Success {
            value: args["text"].clone(),
        }),
    );
    registry
}

/// turn 运行中通过 external_events 通道入队 steer（进程边界组件的合法路径）。
fn steer_source(flag: Arc<AtomicUsize>, text: &'static str) -> impl Fn() -> Vec<Event> {
    move || {
        if flag.fetch_add(1, Ordering::SeqCst) == 0 {
            // 第一次被调用发生在第 1 次采样之后：此时入队 steer
            vec![Event::UserInputQueued {
                text: text.to_string(),
            }]
        } else {
            Vec::new()
        }
    }
}

#[test]
fn queued_input_is_visible_at_next_round_boundary_without_breaking_pairs() {
    let path = tmp("steer.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let model = Capturing {
        script: Mutex::new(vec![Some("call_1".into()), None]),
        seen: Mutex::new(Vec::new()),
    };
    let registry = echo_registry();
    let events_source = steer_source(Arc::new(AtomicUsize::new(0)), "优先处理 X");
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.external_events = Some(&events_source);
    lp.inbox.push("开始");
    assert_eq!(lp.run_turn(), 1);

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // 第 1 次采样：看不到 steer
    assert!(!seen[0]
        .iter()
        .any(|message| message.content.contains("优先处理 X")));
    // 第 2 次采样（工具结果之后的轮边界）：steer 已按序进入历史
    assert!(
        seen[1]
            .iter()
            .any(|message| message.content == "优先处理 X"),
        "steer 必须在下一采样轮前可见"
    );
    // steer 位于工具结果之后、新采样之前（模型表面顺序一致性）
    let positions: Vec<usize> = seen[1]
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            if message.content == "优先处理 X" {
                Some(index)
            } else {
                None
            }
        })
        .collect();
    let tool_result_position = seen[1]
        .iter()
        .position(|message| message.content.contains("\"success\""))
        .expect("tool result must be in history");
    assert!(positions[0] > tool_result_position);

    // 事件流：UserInputQueued 落盘；call/result 配对完整
    let events = replay(&path).unwrap();
    assert!(events
        .iter()
        .any(|sequenced| matches!(&sequenced.event, Event::UserInputQueued { .. })));
    let requested: Vec<String> = events
        .iter()
        .filter_map(|sequenced| match &sequenced.event {
            Event::ToolCallRequested { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let answered: Vec<String> = events
        .iter()
        .filter_map(|sequenced| match &sequenced.event {
            Event::ToolResultAdded { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(requested, answered);
    std::fs::remove_file(&path).ok();
}

#[test]
fn projection_defers_queued_input_until_tool_batch_closes() {
    // 批次中途入队：投影不得把 User 消息插进 tool_calls 与 tool_result 之间
    let events = vec![
        SequencedEvent {
            seq: 0,
            event: Event::UserMessage {
                text: "开始".into(),
            },
        },
        SequencedEvent {
            seq: 1,
            event: Event::ModelToolCallsRequested {
                request_id: "r1".into(),
                calls: vec![ModelToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: "{}".into(),
                }],
            },
        },
        SequencedEvent {
            seq: 2,
            event: Event::UserInputQueued {
                text: "中途插入".into(),
            },
        },
        SequencedEvent {
            seq: 3,
            event: Event::ToolResultAdded {
                call_id: "c1".into(),
                outcome: ToolOutcome::Success {
                    value: serde_json::json!("ok"),
                },
            },
        },
        SequencedEvent {
            seq: 4,
            event: Event::AssistantMessage {
                text: "完成".into(),
            },
        },
    ];
    let surface = project_model_surface(&events).unwrap();
    let shapes: Vec<String> = surface
        .iter()
        .map(|entry| match &entry.message {
            protocol::ModelSurfaceMessage::User { .. } => "user".into(),
            protocol::ModelSurfaceMessage::Assistant { .. } => "assistant".into(),
            protocol::ModelSurfaceMessage::AssistantToolCalls { .. } => "tool_calls".into(),
            protocol::ModelSurfaceMessage::ToolResult { .. } => "tool_result".into(),
            protocol::ModelSurfaceMessage::SystemSummary { .. } => "system".into(),
        })
        .collect();
    // 中途入队的 User 出现在 tool_result 闭合之后、assistant 之前
    assert_eq!(
        shapes,
        vec!["user", "tool_calls", "tool_result", "user", "assistant"]
    );
    let text = match &surface[3].message {
        protocol::ModelSurfaceMessage::User { text } => text.clone(),
        other => panic!("expected deferred user message, got {other:?}"),
    };
    assert_eq!(text, "中途插入");
}

#[test]
fn leftover_queued_input_flows_into_next_turn() {
    // turn 结束后才入队的输入：下一 turn 首个采样轮边界吸收
    let path = tmp("leftover.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let model = Capturing {
        script: Mutex::new(vec![None, None]),
        seen: Mutex::new(Vec::new()),
    };
    let registry = echo_registry();
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.inbox.push("第一轮");
    assert_eq!(lp.run_turn(), 1);
    // turn 之间的 steer
    lp.enqueue_input("补充说明 Y").unwrap();
    lp.inbox.push("第二轮");
    assert_eq!(lp.run_turn(), 1);

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // 第二个 turn 的第 1 次采样（总第 2 次）：上一 turn 的残留 steer 已在历史中
    assert!(
        seen[1]
            .iter()
            .any(|message| message.content == "补充说明 Y"),
        "跨 turn 残留的排队输入必须在下一 turn 被吸收"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn enqueue_rejects_blank_input() {
    let path = tmp("blank.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let registry = echo_registry();
    let model = Capturing {
        script: Mutex::new(vec![]),
        seen: Mutex::new(Vec::new()),
    };
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    assert_eq!(
        lp.enqueue_input("   ").unwrap_err().code,
        ErrorCode::ToolArgsInvalid
    );
    std::fs::remove_file(&path).ok();
}
