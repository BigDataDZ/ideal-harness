//! TASK-702 验收：循环防护——连续等参调用先提醒后拒绝，拒绝不触发 handler，配对完整。

use agent_loop::{AgentLoop, LoopGuard};
use model_provider::{ChatMessage, ChatModel, ChatReply, ToolCallRequest};
use protocol::{ErrorCode, ErrorEnvelope, Event, ModelCallSpec, SequencedEvent, ToolOutcome};
use session::{replay, JsonlSession};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tools::ToolRegistry;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-loop-guard-{}-{name}", std::process::id()))
}

fn spec() -> ModelCallSpec {
    ModelCallSpec {
        model: "mock-model".into(),
        base_url: "http://localhost".into(),
        temperature: None,
    }
}

/// 每轮都调用同一工具、同一参数；call id 唯一递增（投影要求批次内 id 可追溯）。
struct AlwaysSame(Arc<AtomicUsize>);

impl ChatModel for AlwaysSame {
    fn stream_chat(
        &self,
        _: &ModelCallSpec,
        _: &[ChatMessage],
        _: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatReply {
            text: "再试一次".into(),
            finish_reason: Some("tool_calls".into()),
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{n}"),
                name: "echo".into(),
                arguments: r#"{"text":"same"}"#.into(),
            }],
            usage: None,
        })
    }
}

#[test]
fn loop_guard_reminds_then_rejects_without_invoking_handler() {
    let path = tmp("guard.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut registry = ToolRegistry::default();
    let invocations = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&invocations);
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
        Box::new(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) + 1 > 2 {
                panic!("handler must not run after loop guard rejection");
            }
            ToolOutcome::Success {
                value: serde_json::json!({ "echoed": "same" }),
            }
        }),
    );
    let model = AlwaysSame(Arc::new(AtomicUsize::new(0)));
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.loop_guard = Some(LoopGuard {
        remind_after: 2,
        reject_after: 3,
    });
    lp.max_tool_rounds = 5;
    lp.inbox.push("反复调用");
    assert_eq!(
        lp.run_turn(),
        0,
        "连续等参调用会被拒绝到 max_tool_rounds 终结"
    );

    let events = replay(&path).unwrap();
    let outcomes: Vec<ToolOutcome> = events
        .iter()
        .filter_map(|sequenced| match &sequenced.event {
            Event::ToolResultAdded { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 5, "{outcomes:?}");
    // 第 1 次：正常结果；第 2 次：结果 + 提醒；第 3~5 次：拒绝
    match &outcomes[0] {
        ToolOutcome::Success { value } => assert_eq!(value["echoed"], "same"),
        other => panic!("expected plain success, got {other:?}"),
    }
    match &outcomes[1] {
        ToolOutcome::Success { value } => {
            assert_eq!(value["result"]["echoed"], "same");
            assert!(value["guard_notice"]
                .as_str()
                .unwrap()
                .contains("repeated identical tool call #2"));
        }
        other => panic!("expected wrapped success, got {other:?}"),
    }
    for outcome in &outcomes[2..] {
        match outcome {
            ToolOutcome::Failure { error } => {
                assert_eq!(error.code, ErrorCode::ToolLoopDetected);
                assert!(
                    error.message.contains("repeated 3 times")
                        || error.message.contains("repeated 4 times")
                        || error.message.contains("repeated 5 times")
                );
            }
            other => panic!("expected loop-detected failure, got {other:?}"),
        }
    }
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "拒绝分支不得触发 handler"
    );
    // turn 以 max_tool_rounds 终结；tool_call/result 全程配对
    assert!(matches!(
        events.last().unwrap().event,
        Event::TurnAborted { .. }
    ));
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
fn loop_guard_absent_keeps_repeating_calls() {
    // 非回归：未配置护栏时行为与既有完全一致（护栏是增强件，不是 fail-closed 安全件）
    let path = tmp("no-guard.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
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
        Box::new(|_| ToolOutcome::Success {
            value: serde_json::json!({ "echoed": "same" }),
        }),
    );
    let model = AlwaysSame(Arc::new(AtomicUsize::new(0)));
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.max_tool_rounds = 3;
    lp.inbox.push("x");
    assert_eq!(lp.run_turn(), 0);
    let events: Vec<SequencedEvent> = replay(&path).unwrap();
    let successes = events
        .iter()
        .filter(|sequenced| {
            matches!(
                &sequenced.event,
                Event::ToolResultAdded {
                    outcome: ToolOutcome::Success { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(successes, 3, "无护栏时每次调用都执行");
    std::fs::remove_file(&path).ok();
}

#[test]
fn distinct_arguments_reset_the_consecutive_counter() {
    // 参数变化即视为新调用：拒绝只针对完全相同的等参重复
    let path = tmp("vary-args.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
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
    struct Varying(Arc<AtomicUsize>);
    impl ChatModel for Varying {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatReply {
                text: String::new(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: format!("call_{n}"),
                    name: "echo".into(),
                    arguments: format!(r#"{{"text":"v{n}"}}"#),
                }],
                usage: None,
            })
        }
    }
    let model = Varying(Arc::new(AtomicUsize::new(0)));
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.loop_guard = Some(LoopGuard {
        remind_after: 2,
        reject_after: 3,
    });
    lp.max_tool_rounds = 4;
    lp.inbox.push("x");
    assert_eq!(lp.run_turn(), 0);
    let events = replay(&path).unwrap();
    let failures = events
        .iter()
        .filter(|sequenced| {
            matches!(
                &sequenced.event,
                Event::ToolResultAdded {
                    outcome: ToolOutcome::Failure { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(failures, 0, "参数每次都不同，循环防护不得触发");
    std::fs::remove_file(&path).ok();
}
