//! P3 / TASK-404：进程内子代理骨架；内部 trace 隔离，父会话只接收最终报告。

use crate::subagent_policy::{validate_delegation, SubagentPolicy, SubagentRequest};
use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use session::SessionStore;

const SUBAGENT_TOOL: &str = "subagent";

/// 一次子代理委派。ID 用于父事件流中的稳定配对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentTask {
    id: String,
    prompt: String,
}

impl SubagentTask {
    pub fn new(id: impl Into<String>, prompt: impl Into<String>) -> Result<Self, ErrorEnvelope> {
        let id = id.into();
        let prompt = prompt.into();
        if id.trim().is_empty() || prompt.trim().is_empty() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "subagent id and prompt must be non-empty",
            ));
        }
        Ok(Self { id, prompt })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

/// 子代理的最终结构化报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentReport {
    pub task_id: String,
    pub text: String,
    pub child_event_count: usize,
}

/// 子代理私有事件区；其中任何事件都不会被隐式复制到父会话。
#[derive(Debug, Default)]
pub struct SubagentTrace {
    events: Vec<Event>,
}

impl SubagentTrace {
    pub fn record(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// 可注入的进程内子代理执行器；故障测试可在隔离 trace 中制造半截执行。
pub trait SubagentRunner {
    fn run(&self, task: &SubagentTask, trace: &mut SubagentTrace) -> Result<String, ErrorEnvelope>;
}

pub(crate) fn run(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    request: &SubagentRequest,
    parent_policy: &SubagentPolicy,
    child_policy: &SubagentPolicy,
    runner: &dyn SubagentRunner,
) -> Result<SubagentReport, ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::ToolCallRequested {
            call_id: task.id.clone(),
            tool: SUBAGENT_TOOL.to_string(),
            args: serde_json::json!({
                "prompt": task.prompt,
                "depth": request.depth(),
                "active_children": request.active_children(),
                "turn_budget": request.turn_budget(),
                "token_budget": request.token_budget(),
                "model": request.model(),
                "tools": request.tools(),
            }),
        },
    )?;

    if let Err(error) = validate_delegation(parent_policy, child_policy, request) {
        append_failure(parent, task, &error)?;
        return Err(error);
    }

    let mut trace = SubagentTrace::default();
    let result = runner.run(task, &mut trace);

    match result {
        Ok(text) => {
            let report = SubagentReport {
                task_id: task.id.clone(),
                text,
                child_event_count: trace.len(),
            };
            append_parent_event(
                parent,
                Event::ToolResultAdded {
                    call_id: task.id.clone(),
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
        Err(error) => {
            append_failure(parent, task, &error)?;
            Err(error)
        }
    }
}

fn append_failure(
    parent: &mut dyn SessionStore,
    task: &SubagentTask,
    error: &ErrorEnvelope,
) -> Result<(), ErrorEnvelope> {
    append_parent_event(
        parent,
        Event::ToolResultAdded {
            call_id: task.id.clone(),
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
            format!("failed to append subagent report event: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentLoop, ModelProvider};
    use protocol::SequencedEvent;
    use session::JsonlSession;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tools::ToolRegistry;

    struct Unused;

    impl ModelProvider for Unused {
        fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
            panic!("parent model must not run")
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-subagent-{}-{name}.jsonl", std::process::id()))
    }

    fn replay(path: &Path) -> Vec<SequencedEvent> {
        session::replay(path).unwrap()
    }

    struct Successful;

    impl SubagentRunner for Successful {
        fn run(
            &self,
            task: &SubagentTask,
            trace: &mut SubagentTrace,
        ) -> Result<String, ErrorEnvelope> {
            trace.record(Event::UserMessage {
                text: task.prompt().to_string(),
            });
            trace.record(Event::AssistantMessage {
                text: "private working note".into(),
            });
            Ok("final finding".into())
        }
    }

    #[test]
    fn successful_report_returns_as_paired_parent_events_only() {
        let path = tmp("success");
        std::fs::remove_file(&path).ok();
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
        let task = SubagentTask::new("research-1", "inspect module").unwrap();

        let report = parent.run_subagent(&task, &Successful).unwrap();
        assert_eq!(report.text, "final finding");
        assert_eq!(report.child_event_count, 2);

        let events = replay(&path);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].event,
            Event::ToolCallRequested { ref tool, .. } if tool == SUBAGENT_TOOL
        ));
        match &events[1].event {
            Event::ToolResultAdded {
                call_id,
                outcome: ToolOutcome::Success { value },
            } => {
                assert_eq!(call_id, "research-1");
                assert_eq!(value["kind"], "subagent_report");
                assert_eq!(value["text"], "final finding");
            }
            other => panic!("expected successful report event, got {other:?}"),
        }
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("private working note"));
        std::fs::remove_file(&path).ok();
    }

    struct FailsAfterPartialWork;

    impl SubagentRunner for FailsAfterPartialWork {
        fn run(
            &self,
            _: &SubagentTask,
            trace: &mut SubagentTrace,
        ) -> Result<String, ErrorEnvelope> {
            trace.record(Event::AssistantMessage {
                text: "partial child output".into(),
            });
            Err(ErrorEnvelope::new(
                ErrorCode::ModelStreamBroken,
                "child stream failed",
            ))
        }
    }

    #[test]
    fn child_failure_does_not_leak_partial_trace_into_parent() {
        let path = tmp("failure");
        std::fs::remove_file(&path).ok();
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
        let task = SubagentTask::new("research-2", "fail safely").unwrap();

        let error = parent
            .run_subagent(&task, &FailsAfterPartialWork)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ModelStreamBroken);

        let events = replay(&path);
        assert_eq!(events.len(), 2, "parent receives one complete audit pair");
        assert!(matches!(events[0].event, Event::ToolCallRequested { .. }));
        assert!(matches!(
            events[1].event,
            Event::ToolResultAdded {
                outcome: ToolOutcome::Failure {
                    error: ErrorEnvelope {
                        code: ErrorCode::ModelStreamBroken,
                        ..
                    }
                },
                ..
            }
        ));
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("partial child output"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_task_is_rejected_before_parent_session_changes() {
        let path = tmp("invalid");
        std::fs::remove_file(&path).ok();
        let session = JsonlSession::create(path.clone()).unwrap();
        let error = SubagentTask::new(" ", "work").unwrap_err();
        assert_eq!(error.code, ErrorCode::ToolArgsInvalid);
        drop(session);
        assert!(replay(&path).is_empty());
        std::fs::remove_file(&path).ok();
    }

    struct CountingRunner<'a>(&'a AtomicUsize);

    impl SubagentRunner for CountingRunner<'_> {
        fn run(&self, _: &SubagentTask, _: &mut SubagentTrace) -> Result<String, ErrorEnvelope> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("must not run".into())
        }
    }

    #[test]
    fn policy_rejection_is_a_complete_parent_pair_and_runner_is_not_called() {
        let path = tmp("policy-reject");
        std::fs::remove_file(&path).ok();
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut parent = AgentLoop::new(&mut session, &tools, &Unused);
        let task = SubagentTask::new("research-3", "stay bounded").unwrap();
        let policy =
            SubagentPolicy::new(1, 1, 2, 100, ["model-a".into()], ["read".into()], []).unwrap();
        let request =
            SubagentRequest::new(2, 0, 2, 100, Some("model-a".into()), ["read".into()]).unwrap();
        let calls = AtomicUsize::new(0);

        let error = parent
            .run_subagent_with_policy(&task, &request, &policy, &policy, &CountingRunner(&calls))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::SandboxDenied);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let events = replay(&path);
        assert_eq!(events.len(), 2);
        let call_id = match &events[0].event {
            Event::ToolCallRequested { call_id, .. } => call_id,
            other => panic!("expected call event, got {other:?}"),
        };
        match &events[1].event {
            Event::ToolResultAdded {
                call_id: result_id,
                outcome: ToolOutcome::Failure { error },
            } => {
                assert_eq!(result_id, call_id);
                assert_eq!(error.code, ErrorCode::SandboxDenied);
            }
            other => panic!("expected failed result event, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }
}
