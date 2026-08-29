use agent_loop::{AgentLoop, McpInvocation, ModelProvider};
use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use session::{replay, JsonlSession, SpillLocator, SpillStore};
use std::path::PathBuf;
use tools::{McpClient, McpServerConfig, ToolRegistry};

struct Unused;

impl ModelProvider for Unused {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        panic!("parent model must not run")
    }
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-mcp-bridge-{}-{name}", std::process::id()))
}

fn config(mode: &str) -> McpServerConfig {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/tests/fixtures");
    if cfg!(windows) {
        McpServerConfig {
            source: "fixture".into(),
            program: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                fixture
                    .join("mcp_server.ps1")
                    .to_string_lossy()
                    .into_owned(),
                mode.into(),
            ],
            max_output_bytes: 32,
        }
    } else {
        McpServerConfig {
            source: "fixture".into(),
            program: "sh".into(),
            args: vec![
                fixture.join("mcp_server.sh").to_string_lossy().into_owned(),
                mode.into(),
            ],
            max_output_bytes: 32,
        }
    }
}

#[test]
fn oversized_result_spills_and_is_retrievable_with_approval_audit() {
    let session_path = tmp("spill.jsonl");
    let spill_root = tmp("spill-root");
    let _ = std::fs::remove_file(&session_path);
    let _ = std::fs::remove_dir_all(&spill_root);
    let mut session = JsonlSession::create(session_path.clone()).unwrap();
    let registry = ToolRegistry::default();
    let mut agent = AgentLoop::new(&mut session, &registry, &Unused);
    let mut client = McpClient::connect(config("normal")).unwrap();
    let invocation = McpInvocation::new(
        "mcp-1",
        "verbose",
        serde_json::json!({}),
        true,
        spill_root.clone(),
    )
    .unwrap();
    let outcome = agent.run_mcp_tool(&mut client, &invocation).unwrap();
    let value = match outcome {
        ToolOutcome::Success { value } => value,
        other => panic!("expected success, got {other:?}"),
    };
    let locator = SpillLocator::parse(value["output"]["locator"].as_str().unwrap()).unwrap();
    let store = SpillStore::create(spill_root.clone(), 12, 12).unwrap();
    assert_eq!(store.retrieve(&locator).unwrap(), "x".repeat(64));
    let events = replay(&session_path).unwrap();
    assert_eq!(events.len(), 3);
    assert!(matches!(
        events[1].event,
        Event::ApprovalDecided { approved: true, .. }
    ));
    assert!(matches!(events[2].event, Event::ToolResultAdded { .. }));
    let _ = std::fs::remove_file(session_path);
    let _ = std::fs::remove_dir_all(spill_root);
}

#[test]
fn rejection_protocol_error_and_child_exit_each_leave_complete_pair() {
    for (mode, approved, code) in [
        ("malformed", false, ErrorCode::ApprovalRejected),
        ("malformed", true, ErrorCode::Internal),
        ("exit_on_call", true, ErrorCode::Internal),
    ] {
        let session_path = tmp(&format!("failure-{mode}-{approved}.jsonl"));
        let spill_root = tmp(&format!("failure-{mode}-{approved}-spill"));
        let _ = std::fs::remove_file(&session_path);
        let mut session = JsonlSession::create(session_path.clone()).unwrap();
        let registry = ToolRegistry::default();
        let mut agent = AgentLoop::new(&mut session, &registry, &Unused);
        let mut client = McpClient::connect(config(mode)).unwrap();
        let invocation = McpInvocation::new(
            "mcp-fail",
            "echo",
            serde_json::json!({ "text": "x" }),
            approved,
            spill_root,
        )
        .unwrap();
        assert_eq!(
            agent
                .run_mcp_tool(&mut client, &invocation)
                .unwrap_err()
                .code,
            code
        );
        let events = replay(&session_path).unwrap();
        assert_eq!(events.len(), 3);
        let call_id = match &events[0].event {
            Event::ToolCallRequested { call_id, .. } => call_id,
            other => panic!("expected call, got {other:?}"),
        };
        match &events[2].event {
            Event::ToolResultAdded {
                call_id: result_id,
                outcome: ToolOutcome::Failure { error },
            } => {
                assert_eq!(call_id, result_id);
                assert_eq!(error.code, code);
            }
            other => panic!("expected failure result, got {other:?}"),
        }
        let _ = std::fs::remove_file(session_path);
    }
}
