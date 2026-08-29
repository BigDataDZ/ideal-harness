//! TASK-412：subagent 成功与取消生命周期的 canonical JSONL 场景快照。

use agent_loop::{
    AgentLoop, ModelProvider, SubagentCancellation, SubagentDelegation, SubagentPolicy,
    SubagentRequest, SubagentRunner, SubagentTask, SubagentTrace,
};
use protocol::{ErrorEnvelope, Event, SequencedEvent, SubagentReportDelivery};
use session::{replay, JsonlSession};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tools::ToolRegistry;

struct Unused;

impl ModelProvider for Unused {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        panic!("parent model must not run")
    }
}

struct Success<'a>(&'a AtomicUsize);

impl SubagentRunner for Success<'_> {
    fn run(
        &self,
        _: &SubagentTask,
        trace: &mut SubagentTrace,
        _: &SubagentCancellation,
    ) -> Result<String, ErrorEnvelope> {
        self.0.fetch_add(1, Ordering::SeqCst);
        trace.record(Event::AssistantMessage {
            text: "private".into(),
        });
        Ok("report".into())
    }
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ih-snapshot-agent-{}-{name}.jsonl",
        std::process::id()
    ))
}

fn assert_snapshot(name: &str, events: &[SequencedEvent]) {
    let actual = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/event-traces")
        .join(format!("{name}.jsonl"));
    let expected = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| {
            panic!(
                "missing snapshot {}: {error}\nactual:\n{actual}",
                fixture.display()
            )
        })
        .replace("\r\n", "\n");
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(left, right)| left != right)
            .map_or(1, |index| index + 1);
        panic!("snapshot {name} mismatch at line {line}\nexpected:\n{expected}\nactual:\n{actual}");
    }
}

fn policy() -> SubagentPolicy {
    SubagentPolicy::local_default()
}

fn request() -> SubagentRequest {
    SubagentRequest::local_default()
}

#[test]
fn subagent_success_trace_matches_snapshot() {
    let path = tmp("success");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let tools = ToolRegistry::default();
    let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
    let task = SubagentTask::with_lineage("task-ok", "work", "root", "child-ok").unwrap();
    let calls = AtomicUsize::new(0);
    parent.run_subagent(&task, &Success(&calls)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_snapshot("subagent-success", &replay(&path).unwrap());
    std::fs::remove_file(path).ok();
}

#[test]
fn subagent_cancelled_trace_matches_snapshot() {
    let path = tmp("cancelled");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let tools = ToolRegistry::default();
    let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
    let task = SubagentTask::with_lineage("task-cancel", "work", "root", "child-cancel").unwrap();
    let cancellation = SubagentCancellation::new();
    cancellation.cancel("parent stopped").unwrap();
    let request = request();
    let policy = policy();
    let delegation = SubagentDelegation::new(
        &request,
        &policy,
        &policy,
        SubagentReportDelivery::Quiet,
        &cancellation,
    );
    let calls = AtomicUsize::new(0);
    let error = parent
        .run_subagent_lifecycle(&task, &delegation, &Success(&calls))
        .unwrap_err();
    assert_eq!(error.code, protocol::ErrorCode::SubagentCancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_snapshot("subagent-cancelled", &replay(&path).unwrap());
    std::fs::remove_file(path).ok();
}
