//! TASK-607 验收：工具结果安全中间件——检查/脱敏/拒绝，
//! 插件来源结果在中间件缺席或失败时 fail-closed 并留 Event。

use agent_loop::{AgentLoop, ToolResultContext, ToolResultDecision, ToolResultMiddleware};
use model_provider::{ChatMessage, ChatModel, ChatReply, ToolCallRequest};
use protocol::{ErrorCode, ErrorEnvelope, Event, ModelCallSpec, SequencedEvent, ToolOutcome};
use session::{replay, JsonlSession};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tools::{content_hash, PluginCatalog, ToolRegistry, ToolSpec};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-plugin-mw-{}-{name}", std::process::id()))
}

fn manifest_json(name: &str, hash: &str) -> String {
    serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "payload": "payload.json",
        "hash": hash,
        "tools": [{
            "name": format!("{name}_hello"),
            "description": "Greet via plugin",
            "parameters_schema": { "type": "object", "properties": {} }
        }]
    })
    .to_string()
}

const PLUGIN_PAYLOAD: &str = r#"{"secret":"plugin-data","safe":"hello"}"#;

/// 带一个 greeter 插件的临时工作区。
fn make_workspace(name: &str) -> PathBuf {
    let root = tmp(name);
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join(".harness/plugins/greeter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        manifest_json("greeter", &content_hash(PLUGIN_PAYLOAD.as_bytes())),
    )
    .unwrap();
    std::fs::write(dir.join("payload.json"), PLUGIN_PAYLOAD).unwrap();
    root
}

struct Scripted {
    replies: Mutex<Vec<ChatReply>>,
    seen: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ChatModel for Scripted {
    fn stream_chat(
        &self,
        _: &ModelCallSpec,
        msgs: &[ChatMessage],
        _: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope> {
        self.seen.lock().unwrap().push(msgs.to_vec());
        let mut queue = self.replies.lock().unwrap();
        Ok(queue.remove(0))
    }
}

fn tool_call_reply() -> ChatReply {
    ChatReply {
        text: "让我查一下".into(),
        finish_reason: Some("tool_calls".into()),
        tool_calls: vec![ToolCallRequest {
            id: "call_1".into(),
            name: "greeter_hello".into(),
            arguments: "{}".into(),
        }],
        usage: None,
    }
}

fn text_reply() -> ChatReply {
    ChatReply {
        text: "完成".into(),
        finish_reason: Some("stop".into()),
        tool_calls: vec![],
        usage: None,
    }
}

struct Fixed {
    decision: ToolResultDecision,
    fail: bool,
}

impl ToolResultMiddleware for Fixed {
    fn inspect(&self, _: &ToolResultContext<'_>) -> Result<ToolResultDecision, ErrorEnvelope> {
        if self.fail {
            return Err(ErrorEnvelope::new(
                ErrorCode::Internal,
                "middleware exploded",
            ));
        }
        Ok(self.decision.clone())
    }
}

fn redacted_secret() -> ToolResultDecision {
    ToolResultDecision::Redact(ToolOutcome::Success {
        value: serde_json::json!({ "secret": "[redacted]", "safe": "hello" }),
    })
}

fn plugin_registry(ws: &Path) -> (Arc<PluginCatalog>, ToolRegistry) {
    let catalog = Arc::new(PluginCatalog::discover(ws).unwrap());
    let mut registry = ToolRegistry::default();
    catalog
        .bind_static_tools(&mut registry, "greeter")
        .expect("bind plugin tools");
    (catalog, registry)
}

/// 跑完一轮「工具调用 → 文本答复」，返回事件流与模型每轮看到的消息。
fn run_plugin_turn(
    case: &str,
    middleware: Option<&dyn ToolResultMiddleware>,
) -> (Vec<SequencedEvent>, Vec<Vec<ChatMessage>>) {
    let ws = make_workspace(case);
    let (_catalog, registry) = plugin_registry(&ws);
    let model = Scripted {
        replies: Mutex::new(vec![tool_call_reply(), text_reply()]),
        seen: Mutex::new(Vec::new()),
    };
    let path = tmp(&format!("{case}.jsonl"));
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.result_middleware = middleware;
    lp.inbox.push("调用插件");
    assert_eq!(lp.run_turn(), 1, "{case}: turn 应完成");
    let events = replay(&path).unwrap();
    let seen = model.seen.lock().unwrap().clone();
    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(ws).ok();
    (events, seen)
}

fn spec() -> ModelCallSpec {
    ModelCallSpec {
        model: "mock-model".into(),
        base_url: "http://localhost".into(),
        temperature: None,
    }
}

fn plugin_result(events: &[SequencedEvent]) -> ToolOutcome {
    events
        .iter()
        .find_map(|sequenced| match &sequenced.event {
            Event::ToolResultAdded { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("ToolResultAdded must exist")
}

fn model_view(seen: &[Vec<ChatMessage>]) -> String {
    seen.last()
        .expect("model must have seen at least one sample")
        .iter()
        .map(|message| message.content.clone())
        .collect()
}

#[test]
fn plugin_result_without_middleware_fails_closed_and_leaves_event() {
    let (events, seen) = run_plugin_turn("absent", None);
    match plugin_result(&events) {
        ToolOutcome::Failure { error } => {
            assert_eq!(error.code, ErrorCode::Internal);
            assert!(error.message.contains("middleware is absent"));
        }
        other => panic!("expected fail-closed outcome, got {other:?}"),
    }
    let view = model_view(&seen);
    assert!(
        view.contains("middleware is absent"),
        "模型应看到稳定失败以便自纠"
    );
    assert!(
        !view.contains("plugin-data"),
        "未受监管的插件内容不得进入模型表面"
    );
    assert!(matches!(
        events.last().unwrap().event,
        Event::TurnCompleted { .. }
    ));
}

#[test]
fn middleware_allow_redact_reject_and_failure_each_leave_event() {
    // Allow：payload 原样进模型表面
    let (events, seen) = run_plugin_turn(
        "allow",
        Some(&Fixed {
            decision: ToolResultDecision::Allow,
            fail: false,
        }),
    );
    match plugin_result(&events) {
        ToolOutcome::Success { value } => assert_eq!(value["secret"], "plugin-data"),
        other => panic!("expected allowed payload, got {other:?}"),
    }
    assert!(model_view(&seen).contains("plugin-data"));

    // Redact：事件流与模型表面只见替换后的结果
    let (events, seen) = run_plugin_turn(
        "redact",
        Some(&Fixed {
            decision: redacted_secret(),
            fail: false,
        }),
    );
    match plugin_result(&events) {
        ToolOutcome::Success { value } => {
            assert_eq!(value["secret"], "[redacted]");
            assert_eq!(value["safe"], "hello");
        }
        other => panic!("expected redacted payload, got {other:?}"),
    }
    let view = model_view(&seen);
    assert!(view.contains("[redacted]"));
    assert!(!view.contains("plugin-data"), "脱敏必须先于模型表面");

    // Reject：中间件选择的稳定错误码原样回给模型
    let (events, _) = run_plugin_turn(
        "reject",
        Some(&Fixed {
            decision: ToolResultDecision::Reject(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "plugin result rejected by security policy",
            )),
            fail: false,
        }),
    );
    match plugin_result(&events) {
        ToolOutcome::Failure { error } => {
            assert_eq!(error.code, ErrorCode::SandboxDenied);
            assert!(error.message.contains("security policy"));
        }
        other => panic!("expected rejected outcome, got {other:?}"),
    }

    // 中间件自身失败：fail-closed 为 Internal
    let (events, _) = run_plugin_turn(
        "middleware-error",
        Some(&Fixed {
            decision: ToolResultDecision::Allow,
            fail: true,
        }),
    );
    match plugin_result(&events) {
        ToolOutcome::Failure { error } => {
            assert_eq!(error.code, ErrorCode::Internal);
            assert!(error.message.contains("middleware failed"));
        }
        other => panic!("expected fail-closed outcome, got {other:?}"),
    }
}

#[test]
fn call_result_pairing_survives_middleware_rejection() {
    let (events, _) = run_plugin_turn(
        "pairing",
        Some(&Fixed {
            decision: ToolResultDecision::Reject(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "rejected",
            )),
            fail: false,
        }),
    );
    let requested = events
        .iter()
        .filter_map(|sequenced| match &sequenced.event {
            Event::ToolCallRequested { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let answered = events
        .iter()
        .filter_map(|sequenced| match &sequenced.event {
            Event::ToolResultAdded { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requested, vec!["call_1".to_string()]);
    assert_eq!(answered, vec!["call_1".to_string()]);
}

#[test]
fn builtin_tool_result_flows_without_middleware() {
    // 非插件来源（内置工具）不受「中间件缺席 fail-closed」约束——非回归保护
    let ws = make_workspace("builtin");
    let (_catalog, mut registry) = plugin_registry(&ws);
    registry.register(
        ToolSpec {
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
    let model = Scripted {
        replies: Mutex::new(vec![
            ChatReply {
                text: String::new(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_2".into(),
                    name: "echo".into(),
                    arguments: r#"{"text":"hi"}"#.into(),
                }],
                usage: None,
            },
            text_reply(),
        ]),
        seen: Mutex::new(Vec::new()),
    };
    let path = tmp("builtin.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.inbox.push("echo 一下");
    assert_eq!(lp.run_turn(), 1);
    let events = replay(&path).unwrap();
    match plugin_result(&events) {
        ToolOutcome::Success { value } => assert_eq!(value, "hi"),
        other => panic!("expected builtin success, got {other:?}"),
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(ws).ok();
}
