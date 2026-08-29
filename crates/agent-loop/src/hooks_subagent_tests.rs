//! TASK-503 subagent-stopped Hook acceptance test.

use crate::{
    AgentLoop, HookContext, HookPoint, HookRegistry, HookResult, ModelProvider,
    SubagentCancellation, SubagentDelegation, SubagentPolicy, SubagentRequest, SubagentRunner,
    SubagentTask, SubagentTrace,
};
use protocol::{ErrorCode, ErrorEnvelope, Event, SubagentReportDelivery};
use session::JsonlSession;
use tools::ToolRegistry;

struct Unused;

impl ModelProvider for Unused {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        panic!("parent model must not run")
    }
}

struct MustNotRun;

impl SubagentRunner for MustNotRun {
    fn run(
        &self,
        _: &SubagentTask,
        _: &mut SubagentTrace,
        _: &SubagentCancellation,
    ) -> Result<String, ErrorEnvelope> {
        panic!("cancelled subagent runner must not run")
    }
}

fn allow(_: &HookContext) -> Result<HookResult, ErrorEnvelope> {
    Ok(HookResult::allow("ok"))
}

#[test]
fn cancelled_subagent_closes_original_pair_before_stopped_hook() {
    let path = std::env::temp_dir().join(format!(
        "ih-hooks-{}-subagent-cancel.jsonl",
        std::process::id()
    ));
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let tools = ToolRegistry::default();
    let mut hooks = HookRegistry::new([HookPoint::SubagentStopped]);
    hooks
        .register(HookPoint::SubagentStopped, "cleanup", allow)
        .unwrap();
    let mut agent = AgentLoop::new(&mut session, &tools, &Unused);
    agent.hooks = Some(&hooks);
    let task = SubagentTask::with_lineage("task", "work", "root", "child").unwrap();
    let cancellation = SubagentCancellation::new();
    cancellation.cancel("stop").unwrap();
    let request = SubagentRequest::local_default();
    let policy = SubagentPolicy::local_default();
    let delegation = SubagentDelegation::new(
        &request,
        &policy,
        &policy,
        SubagentReportDelivery::Quiet,
        &cancellation,
    );
    assert_eq!(
        agent
            .run_subagent_lifecycle(&task, &delegation, &MustNotRun)
            .unwrap_err()
            .code,
        ErrorCode::SubagentCancelled
    );
    drop(agent);

    let events = session::replay(&path).unwrap();
    let stopped = events
        .iter()
        .position(|event| matches!(event.event, Event::SubagentStopped { .. }))
        .unwrap();
    let original_result = events
        .iter()
        .position(|event| {
            matches!(&event.event, Event::ToolResultAdded { call_id, .. } if call_id == "task")
        })
        .unwrap();
    let hook = events
        .iter()
        .position(|event| {
            matches!(&event.event, Event::ToolCallRequested { tool, .. } if tool.contains("subagent_stopped"))
        })
        .unwrap();
    assert!(stopped < original_result && original_result < hook);
    std::fs::remove_file(path).ok();
}
