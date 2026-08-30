//! TASK-705 验收：memory_write 经 ToolAudit 落 MemoryRecorded 事件，配对完整。

use agent_loop::AgentLoop;
use model_provider::{ChatMessage, ChatModel, ChatReply, ToolCallRequest};
use protocol::{ErrorEnvelope, Event, ModelCallSpec, ToolOutcome};
use session::{project_memories, replay, JsonlSession};
use std::path::PathBuf;
use std::sync::Mutex;
use tools::{ToolAudit, ToolExecution, ToolRegistry};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-memory-{}-{name}", std::process::id()))
}

fn spec() -> ModelCallSpec {
    ModelCallSpec {
        model: "mock-model".into(),
        base_url: "http://localhost".into(),
        temperature: None,
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
        let mut queue = self.0.lock().unwrap();
        Ok(queue.remove(0))
    }
}

#[test]
fn memory_write_flows_through_audit_to_event_and_projection() {
    let path = tmp("audit.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut registry = ToolRegistry::default();
    registry.register_audited(
        tools::ToolSpec {
            name: "memory_write".into(),
            description: "demo".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(|args| ToolExecution {
            outcome: ToolOutcome::Success {
                value: serde_json::json!({ "recorded": true }),
            },
            audits: vec![ToolAudit::MemoryRecorded {
                text: args["text"].as_str().unwrap_or_default().to_string(),
                tags: vec!["test".to_string()],
                source: protocol::MemorySource::Model,
                scope: protocol::MemoryScope::LineageOnly,
            }],
        }),
    );
    let model = Scripted(Mutex::new(vec![
        ChatReply {
            text: "我来记一下".into(),
            finish_reason: Some("tool_calls".into()),
            tool_calls: vec![ToolCallRequest {
                id: "call_1".into(),
                name: "memory_write".into(),
                arguments: r#"{"text":"用户偏好 Rust","tags":["lang"]}"#.into(),
            }],
            usage: None,
        },
        ChatReply {
            text: "记好了".into(),
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            usage: None,
        },
    ]));
    let mut lp = AgentLoop::with_chat(&mut session, &registry, &model, spec());
    lp.inbox.push("记住这件事");
    assert_eq!(lp.run_turn(), 1);

    let events = replay(&path).unwrap();
    assert!(events.iter().any(|sequenced| matches!(
        &sequenced.event,
        Event::MemoryRecorded { text, .. } if text == "用户偏好 Rust"
    )));
    // 投影能取回该记忆
    let memories = project_memories(&events).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].text, "用户偏好 Rust");
    assert_eq!(memories[0].tags, vec!["test".to_string()]);
    // 配对完整 + turn 完成
    assert!(matches!(
        events.last().unwrap().event,
        Event::TurnCompleted { .. }
    ));
    std::fs::remove_file(&path).ok();
}
