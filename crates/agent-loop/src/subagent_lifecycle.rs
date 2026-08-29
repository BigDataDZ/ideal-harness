//! P3/TASK-410：子代理生命周期、取消传播和报告投递审计。

use crate::subagent::{SubagentReport, SubagentRunner, SubagentTask, SubagentTrace};
use crate::subagent_policy::{validate_delegation, SubagentPolicy, SubagentRequest};
use protocol::{
    ErrorCode, ErrorEnvelope, Event, SubagentOutcome, SubagentReportDelivery, ToolOutcome,
};
use session::SessionStore;
use std::sync::{Arc, Mutex};

const SUBAGENT_TOOL: &str = "subagent";

/// 可克隆的父级取消信号；runner 收到同一状态并可在安全点检查。
#[derive(Clone, Default)]
pub struct SubagentCancellation {
    reason: Arc<Mutex<Option<String>>>,
}

impl SubagentCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self, reason: impl Into<String>) -> Result<(), ErrorEnvelope> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "subagent cancellation reason must not be blank",
            ));
        }
        let mut state = self.reason.lock().map_err(|_| cancellation_state_error())?;
        if state.is_none() {
            *state = Some(reason);
        }
        Ok(())
    }

    pub fn is_cancelled(&self) -> Result<bool, ErrorEnvelope> {
        self.reason
            .lock()
            .map(|reason| reason.is_some())
            .map_err(|_| cancellation_state_error())
    }

    fn reason(&self) -> Result<Option<String>, ErrorEnvelope> {
        self.reason
            .lock()
            .map(|reason| reason.clone())
            .map_err(|_| cancellation_state_error())
    }
}

pub struct SubagentDelegation<'a> {
    request: &'a SubagentRequest,
    parent_policy: &'a SubagentPolicy,
    child_policy: &'a SubagentPolicy,
    delivery: SubagentReportDelivery,
    cancellation: &'a SubagentCancellation,
}

impl<'a> SubagentDelegation<'a> {
    pub fn new(
        request: &'a SubagentRequest,
        parent_policy: &'a SubagentPolicy,
        child_policy: &'a SubagentPolicy,
        delivery: SubagentReportDelivery,
        cancellation: &'a SubagentCancellation,
    ) -> Self {
        Self {
            request,
            parent_policy,
            child_policy,
            delivery,
            cancellation,
        }
    }

    pub fn delivery(&self) -> SubagentReportDelivery {
        self.delivery
    }
}

pub(crate) fn run(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    delegation: &SubagentDelegation<'_>,
    runner: &dyn SubagentRunner,
) -> Result<SubagentReport, ErrorEnvelope> {
    append_call(parent, task, delegation.request)?;
    if let Err(error) = validate_delegation(
        delegation.parent_policy,
        delegation.child_policy,
        delegation.request,
    ) {
        append_failure(parent, task, &error)?;
        return Err(error);
    }

    append_parent_event(
        parent,
        Event::SubagentStarted {
            task_id: task.id().to_string(),
            parent_id: task.parent_id().to_string(),
            child_id: task.child_id().to_string(),
        },
    )?;

    if let Some(reason) = delegation.cancellation.reason()? {
        return finish_cancelled(parent, task, reason);
    }

    let mut trace = SubagentTrace::default();
    let result = runner.run(task, &mut trace, delegation.cancellation);
    if let Some(reason) = delegation.cancellation.reason()? {
        return finish_cancelled(parent, task, reason);
    }

    match result {
        Ok(text) => finish_success(parent, task, trace.len(), text, delegation.delivery),
        Err(error) => {
            append_stopped(parent, task, SubagentOutcome::Failed)?;
            append_failure(parent, task, &error)?;
            Err(error)
        }
    }
}

fn finish_success(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    child_event_count: usize,
    text: String,
    delivery: SubagentReportDelivery,
) -> Result<SubagentReport, ErrorEnvelope> {
    let report = SubagentReport {
        task_id: task.id().to_string(),
        text,
        child_event_count,
    };
    append_parent_event(
        parent,
        Event::SubagentReportDelivered {
            task_id: task.id().to_string(),
            child_id: task.child_id().to_string(),
            delivery,
            text: report.text.clone(),
        },
    )?;
    append_stopped(parent, task, SubagentOutcome::Succeeded)?;
    append_parent_event(
        parent,
        Event::ToolResultAdded {
            call_id: task.id().to_string(),
            outcome: ToolOutcome::Success {
                value: serde_json::json!({
                    "kind": "subagent_report",
                    "task_id": report.task_id,
                    "text": report.text,
                    "child_event_count": report.child_event_count,
                }),
            },
        },
    )?;
    Ok(report)
}

fn finish_cancelled(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    reason: String,
) -> Result<SubagentReport, ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::SubagentCancellationRequested {
            task_id: task.id().to_string(),
            child_id: task.child_id().to_string(),
            reason,
        },
    )?;
    append_stopped(parent, task, SubagentOutcome::Cancelled)?;
    let error = ErrorEnvelope::new(ErrorCode::SubagentCancelled, "subagent cancelled by parent");
    append_failure(parent, task, &error)?;
    Err(error)
}

fn append_call(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    request: &SubagentRequest,
) -> Result<(), ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::ToolCallRequested {
            call_id: task.id().to_string(),
            tool: SUBAGENT_TOOL.to_string(),
            args: serde_json::json!({
                "prompt": task.prompt(),
                "parent_id": task.parent_id(),
                "child_id": task.child_id(),
                "depth": request.depth(),
                "active_children": request.active_children(),
                "turn_budget": request.turn_budget(),
                "token_budget": request.token_budget(),
                "model": request.model(),
                "tools": request.tools(),
            }),
        },
    )
}

fn append_stopped(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    outcome: SubagentOutcome,
) -> Result<(), ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::SubagentStopped {
            task_id: task.id().to_string(),
            child_id: task.child_id().to_string(),
            outcome,
        },
    )
}

fn append_failure(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    error: &ErrorEnvelope,
) -> Result<(), ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::ToolResultAdded {
            call_id: task.id().to_string(),
            outcome: ToolOutcome::Failure {
                error: error.clone(),
            },
        },
    )
}

fn append_parent_event(parent: &mut dyn SessionStore, event: Event) -> Result<(), ErrorEnvelope> {
    parent.append(event).map(|_| ()).map_err(|error| {
        ErrorEnvelope::new(
            ErrorCode::Internal,
            format!("failed to append subagent lifecycle event: {error}"),
        )
    })
}

fn cancellation_state_error() -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, "subagent cancellation state poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentLoop, ModelProvider};
    use protocol::{SubagentOutcome, SubagentReportDelivery};
    use session::{derive_subagent_lineage, replay, JsonlSession};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tools::ToolRegistry;

    struct Unused;

    impl ModelProvider for Unused {
        fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
            panic!("parent model must not run")
        }
    }

    struct Successful<'a>(&'a AtomicUsize);

    impl SubagentRunner for Successful<'_> {
        fn run(
            &self,
            _: &SubagentTask,
            _: &mut SubagentTrace,
            cancellation: &SubagentCancellation,
        ) -> Result<String, ErrorEnvelope> {
            assert!(!cancellation.is_cancelled()?);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("final report".into())
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ih-subagent-life-{}-{name}.jsonl",
            std::process::id()
        ))
    }

    fn policy() -> SubagentPolicy {
        SubagentPolicy::local_default()
    }

    fn request() -> SubagentRequest {
        SubagentRequest::local_default()
    }

    #[test]
    fn parent_cancellation_skips_runner_and_closes_cancelled_lifecycle() {
        let path = tmp("cancel");
        std::fs::remove_file(&path).ok();
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
        let task = SubagentTask::with_lineage("t-cancel", "work", "root", "child-c").unwrap();
        let cancellation = SubagentCancellation::new();
        cancellation.cancel("parent turn interrupted").unwrap();
        let calls = AtomicUsize::new(0);
        let request = request();
        let policy = policy();
        let delegation = SubagentDelegation::new(
            &request,
            &policy,
            &policy,
            SubagentReportDelivery::Quiet,
            &cancellation,
        );
        let error = parent
            .run_subagent_lifecycle(&task, &delegation, &Successful(&calls))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::SubagentCancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let events = replay(&path).unwrap();
        assert_eq!(events.len(), 5);
        assert!(matches!(
            events[2].event,
            Event::SubagentCancellationRequested { .. }
        ));
        assert!(matches!(
            events[3].event,
            Event::SubagentStopped {
                outcome: SubagentOutcome::Cancelled,
                ..
            }
        ));
        let lineage = derive_subagent_lineage(&events).unwrap();
        assert_eq!(lineage[0].parent_id, "root");
        assert_eq!(lineage[0].child_id, "child-c");
        assert_eq!(lineage[0].outcome, SubagentOutcome::Cancelled);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn quiet_never_wakes_and_next_step_activates_only_when_boundary_is_drained() {
        let path = tmp("delivery");
        std::fs::remove_file(&path).ok();
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
        let calls = AtomicUsize::new(0);
        let runner = Successful(&calls);
        let request = request();
        let policy = policy();
        let quiet = SubagentTask::new("t-quiet", "quiet work").unwrap();
        let quiet_cancellation = SubagentCancellation::new();
        let quiet_delegation = SubagentDelegation::new(
            &request,
            &policy,
            &policy,
            SubagentReportDelivery::Quiet,
            &quiet_cancellation,
        );
        parent
            .run_subagent_lifecycle(&quiet, &quiet_delegation, &runner)
            .unwrap();
        assert!(parent.inbox.is_empty());
        assert!(parent.inbox.drain().is_empty());

        let next = SubagentTask::new("t-next", "next work").unwrap();
        let next_cancellation = SubagentCancellation::new();
        let next_delegation = SubagentDelegation::new(
            &request,
            &policy,
            &policy,
            SubagentReportDelivery::NextStep,
            &next_cancellation,
        );
        parent
            .run_subagent_lifecycle(&next, &next_delegation, &runner)
            .unwrap();
        assert!(
            parent.inbox.is_empty(),
            "next-step must wait for the boundary"
        );
        assert_eq!(parent.inbox.drain(), vec!["final report"]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn blank_cancellation_reason_is_rejected_without_cancelling() {
        let cancellation = SubagentCancellation::new();
        assert_eq!(
            cancellation.cancel(" ").unwrap_err().code,
            ErrorCode::ToolArgsInvalid
        );
        assert!(!cancellation.is_cancelled().unwrap());
    }
}
