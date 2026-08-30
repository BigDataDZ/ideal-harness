//! TASK-503 acceptance tests: ordering, fail-closed behavior and complete audit pairs.

use crate::{AgentLoop, HookContext, HookPoint, HookRegistry, HookResult, ModelProvider, Phase};
use model_provider::{ChatMessage, ChatModel, ChatReply, ToolCallRequest};
use protocol::{ErrorCode, ErrorEnvelope, Event, ModelCallSpec, SequencedEvent, ToolOutcome};
use session::JsonlSession;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tools::{ToolRegistry, ToolSpec};

struct Unused;

impl ModelProvider for Unused {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        Err(ErrorEnvelope::new(ErrorCode::Internal, "unused"))
    }
}

struct Broken;

impl ModelProvider for Broken {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        Err(ErrorEnvelope::new(
            ErrorCode::ModelStreamBroken,
            "stream failed",
        ))
    }
}

struct Scripted(Mutex<Vec<ChatReply>>);

impl ChatModel for Scripted {
    fn stream_chat(
        &self,
        _: &ModelCallSpec,
        _: &[ChatMessage],
        _: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope> {
        Ok(self.0.lock().unwrap().remove(0))
    }
}

fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-hooks-{}-{name}.jsonl", std::process::id()))
}

fn open(name: &str) -> (PathBuf, JsonlSession) {
    let path = path(name);
    std::fs::remove_file(&path).ok();
    let session = JsonlSession::create(path.clone()).unwrap();
    (path, session)
}

fn replay(path: &Path) -> Vec<SequencedEvent> {
    session::replay(path).unwrap()
}

fn spec() -> ModelCallSpec {
    ModelCallSpec {
        model: "mock".into(),
        base_url: "http://localhost".into(),
        temperature: None,
    }
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: "echo".into(),
        description: "echo".into(),
        parameters_schema: serde_json::json!({"type":"object"}),
        escalation_capable: false,
    }
}

fn tool_then_text(tool: &str) -> Scripted {
    Scripted(Mutex::new(vec![
        ChatReply {
            text: String::new(),
            finish_reason: Some("tool_calls".into()),
            tool_calls: vec![ToolCallRequest {
                id: "original-call".into(),
                name: tool.into(),
                arguments: "{}".into(),
            }],
            usage: None,
        },
        ChatReply {
            text: "done".into(),
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            usage: None,
        },
    ]))
}

fn allow(_: &HookContext) -> Result<HookResult, ErrorEnvelope> {
    Ok(HookResult::allow("ok"))
}

fn deny(_: &HookContext) -> Result<HookResult, ErrorEnvelope> {
    Ok(HookResult::deny("blocked"))
}

fn hook_tool(event: &Event) -> Option<&str> {
    match event {
        Event::ToolCallRequested { tool, .. } if tool.starts_with("hook:") => Some(tool),
        _ => None,
    }
}

#[test]
fn normal_tool_and_turn_hooks_are_ordered_without_recursion() {
    let (path, mut session) = open("normal");
    let mut tools = ToolRegistry::default();
    tools.register(
        tool_spec(),
        Box::new(|_| ToolOutcome::Success {
            value: serde_json::json!("ok"),
        }),
    );
    let mut hooks = HookRegistry::new([
        HookPoint::PreToolUse,
        HookPoint::PostToolUse,
        HookPoint::TurnCompleted,
    ]);
    hooks
        .register(HookPoint::PreToolUse, "guard", allow)
        .unwrap();
    hooks
        .register(HookPoint::PostToolUse, "audit", allow)
        .unwrap();
    hooks
        .register(HookPoint::TurnCompleted, "finish", allow)
        .unwrap();
    let chat = tool_then_text("echo");
    let mut agent = AgentLoop::with_chat(&mut session, &tools, &chat, spec());
    agent.hooks = Some(&hooks);
    agent.inbox.push("run");
    assert_eq!(agent.run_turn(), 1);
    drop(agent);

    let events = replay(&path);
    let original_call = events
        .iter()
        .position(|event| {
            matches!(&event.event, Event::ToolCallRequested { call_id, .. } if call_id == "original-call")
        })
        .unwrap();
    let pre = events
        .iter()
        .position(|event| hook_tool(&event.event).is_some_and(|tool| tool.contains("pre_tool_use")))
        .unwrap();
    let original_result = events
        .iter()
        .position(|event| {
            matches!(&event.event, Event::ToolResultAdded { call_id, .. } if call_id == "original-call")
        })
        .unwrap();
    let post = events
        .iter()
        .position(|event| {
            hook_tool(&event.event).is_some_and(|tool| tool.contains("post_tool_use"))
        })
        .unwrap();
    let completed_hook = events
        .iter()
        .position(|event| {
            hook_tool(&event.event).is_some_and(|tool| tool.contains("turn_completed"))
        })
        .unwrap();
    let completed = events
        .iter()
        .position(|event| matches!(event.event, Event::TurnCompleted { .. }))
        .unwrap();
    assert!(original_call < pre && pre < original_result && original_result < post);
    assert!(post < completed_hook && completed_hook < completed);
    assert_eq!(
        events
            .iter()
            .filter(|event| hook_tool(&event.event).is_some())
            .count(),
        3,
        "hook audit events must not recursively trigger hooks"
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn failed_tool_still_runs_post_hook_after_original_result() {
    let (path, mut session) = open("tool-failure");
    let tools = ToolRegistry::default();
    let mut hooks = HookRegistry::new([HookPoint::PostToolUse]);
    hooks
        .register(HookPoint::PostToolUse, "audit", allow)
        .unwrap();
    let chat = tool_then_text("unknown");
    let mut agent = AgentLoop::with_chat(&mut session, &tools, &chat, spec());
    agent.hooks = Some(&hooks);
    agent.inbox.push("run");
    assert_eq!(agent.run_turn(), 1);
    drop(agent);

    let events = replay(&path);
    let original_result = events
        .iter()
        .position(|event| {
            matches!(&event.event, Event::ToolResultAdded { call_id, outcome: ToolOutcome::Failure { .. } } if call_id == "original-call")
        })
        .unwrap();
    let post = events
        .iter()
        .position(|event| {
            hook_tool(&event.event).is_some_and(|tool| tool.contains("post_tool_use"))
        })
        .unwrap();
    assert!(original_result < post);
    std::fs::remove_file(path).ok();
}

#[test]
fn required_pre_hook_failure_is_fail_closed_and_keeps_every_pair() {
    let (path, mut session) = open("pre-failure");
    let mut tools = ToolRegistry::default();
    tools.register(
        tool_spec(),
        Box::new(|_| panic!("required pre hook must block dispatch")),
    );
    let mut hooks = HookRegistry::new([HookPoint::PreToolUse]);
    hooks
        .register(HookPoint::PreToolUse, "guard", deny)
        .unwrap();
    let chat = tool_then_text("echo");
    let mut agent = AgentLoop::with_chat(&mut session, &tools, &chat, spec());
    agent.hooks = Some(&hooks);
    agent.inbox.push("run");
    assert_eq!(agent.run_turn(), 0);
    drop(agent);

    let events = replay(&path);
    let calls = events
        .iter()
        .filter(|event| matches!(event.event, Event::ToolCallRequested { .. }))
        .count();
    let results = events
        .iter()
        .filter(|event| matches!(event.event, Event::ToolResultAdded { .. }))
        .count();
    assert_eq!(calls, results);
    assert!(matches!(
        events.last().unwrap().event,
        Event::TurnAborted { .. }
    ));
    std::fs::remove_file(path).ok();
}

#[test]
fn missing_required_completion_hook_aborts_instead_of_completing() {
    let (path, mut session) = open("missing");
    let tools = ToolRegistry::default();
    let hooks = HookRegistry::new([HookPoint::TurnCompleted]);
    struct Echo;
    impl ModelProvider for Echo {
        fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
            Ok("ok".into())
        }
    }
    let mut agent = AgentLoop::new(&mut session, &tools, &Echo);
    agent.hooks = Some(&hooks);
    agent.inbox.push("run");
    agent.run_turn();
    drop(agent);

    let events = replay(&path);
    assert!(events
        .iter()
        .all(|event| !matches!(event.event, Event::TurnCompleted { .. })));
    assert!(matches!(
        events.last().unwrap().event,
        Event::TurnAborted { .. }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolCallRequested { .. }))
            .count(),
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolResultAdded { .. }))
            .count()
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn interruption_hook_runs_before_abort_and_idle_is_rejected() {
    let (path, mut session) = open("interrupt");
    let tools = ToolRegistry::default();
    let mut hooks = HookRegistry::new([HookPoint::TurnInterrupted]);
    hooks
        .register(HookPoint::TurnInterrupted, "cleanup", allow)
        .unwrap();
    let mut agent = AgentLoop::new(&mut session, &tools, &Unused);
    agent.hooks = Some(&hooks);
    assert_eq!(
        agent.interrupt_turn(7, "stop").unwrap_err().code,
        ErrorCode::ToolArgsInvalid
    );
    agent.phase = Phase::Running;
    agent.interrupt_turn(7, "stop").unwrap();
    drop(agent);

    let events = replay(&path);
    assert!(hook_tool(&events[0].event).is_some_and(|tool| tool.contains("turn_interrupted")));
    assert!(matches!(events[1].event, Event::ToolResultAdded { .. }));
    assert!(matches!(events[2].event, Event::TurnAborted { .. }));
    std::fs::remove_file(path).ok();
}

#[test]
fn failed_turn_hook_runs_after_error_and_before_abort() {
    let (path, mut session) = open("turn-failed");
    let tools = ToolRegistry::default();
    let mut hooks = HookRegistry::new([HookPoint::TurnFailed]);
    hooks
        .register(HookPoint::TurnFailed, "failure-audit", allow)
        .unwrap();
    let mut agent = AgentLoop::new(&mut session, &tools, &Broken);
    agent.hooks = Some(&hooks);
    agent.inbox.push("run");
    assert_eq!(agent.run_turn(), 0);
    drop(agent);

    let events = replay(&path);
    let hook = events
        .iter()
        .position(|event| hook_tool(&event.event).is_some_and(|tool| tool.contains("turn_failed")))
        .unwrap();
    let abort = events
        .iter()
        .position(|event| matches!(event.event, Event::TurnAborted { .. }))
        .unwrap();
    assert!(hook < abort);
    std::fs::remove_file(path).ok();
}

#[test]
fn registry_rejects_duplicate_names() {
    let mut hooks = HookRegistry::default();
    hooks
        .register(HookPoint::TurnFailed, "audit", allow)
        .unwrap();
    assert_eq!(
        hooks
            .register(HookPoint::TurnFailed, "audit", allow)
            .unwrap_err()
            .code,
        ErrorCode::ToolArgsInvalid
    );
}
