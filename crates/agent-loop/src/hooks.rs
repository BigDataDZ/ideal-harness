//! P3/TASK-503：同步 Hook 生命周期与成对审计；Hook 只能返回结果，不能写 session。

use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use session::SessionStore;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

/// AgentLoop 支持的最小 Hook 生命周期点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookPoint {
    PreToolUse,
    PostToolUse,
    TurnCompleted,
    TurnFailed,
    TurnInterrupted,
    SubagentStopped,
}

impl HookPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::TurnCompleted => "turn_completed",
            Self::TurnFailed => "turn_failed",
            Self::TurnInterrupted => "turn_interrupted",
            Self::SubagentStopped => "subagent_stopped",
        }
    }
}

/// 传给 Hook 的只读上下文；刻意不暴露 SessionStore。
#[derive(Debug, Clone, PartialEq)]
pub struct HookContext {
    point: HookPoint,
    turn_id: Option<u64>,
    call_id: Option<String>,
    tool: Option<String>,
    detail: Option<String>,
    outcome: Option<ToolOutcome>,
}

impl HookContext {
    pub(crate) fn tool(
        point: HookPoint,
        turn_id: Option<u64>,
        call_id: impl Into<String>,
        tool: impl Into<String>,
        outcome: Option<ToolOutcome>,
    ) -> Self {
        debug_assert!(matches!(
            point,
            HookPoint::PreToolUse | HookPoint::PostToolUse
        ));
        Self {
            point,
            turn_id,
            call_id: Some(call_id.into()),
            tool: Some(tool.into()),
            detail: None,
            outcome,
        }
    }

    pub(crate) fn turn(point: HookPoint, turn_id: u64, detail: Option<String>) -> Self {
        debug_assert!(matches!(
            point,
            HookPoint::TurnCompleted | HookPoint::TurnFailed | HookPoint::TurnInterrupted
        ));
        Self {
            point,
            turn_id: Some(turn_id),
            call_id: None,
            tool: None,
            detail,
            outcome: None,
        }
    }

    pub(crate) fn subagent(task_id: impl Into<String>, outcome: ToolOutcome) -> Self {
        let task_id = task_id.into();
        Self {
            point: HookPoint::SubagentStopped,
            turn_id: None,
            call_id: Some(task_id),
            tool: Some("subagent".to_string()),
            detail: None,
            outcome: Some(outcome),
        }
    }

    pub fn point(&self) -> HookPoint {
        self.point
    }

    pub fn turn_id(&self) -> Option<u64> {
        self.turn_id
    }

    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.tool.as_deref()
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn outcome(&self) -> Option<&ToolOutcome> {
        self.outcome.as_ref()
    }

    fn event_args(&self) -> serde_json::Value {
        serde_json::json!({
            "point": self.point.as_str(),
            "turn_id": self.turn_id,
            "call_id": self.call_id,
            "tool": self.tool,
            "detail": self.detail,
            "outcome": self.outcome,
        })
    }
}

/// Hook 的结构化决策。`allowed=false` 表示拒绝当前生命周期动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookResult {
    pub allowed: bool,
    pub note: String,
}

impl HookResult {
    pub fn allow(note: impl Into<String>) -> Self {
        Self {
            allowed: true,
            note: note.into(),
        }
    }

    pub fn deny(note: impl Into<String>) -> Self {
        Self {
            allowed: false,
            note: note.into(),
        }
    }
}

/// Hook 只能读取上下文并返回决策，无法直接接触会话真相源。
pub trait Hook {
    fn run(&self, context: &HookContext) -> Result<HookResult, ErrorEnvelope>;
}

impl<F> Hook for F
where
    F: Fn(&HookContext) -> Result<HookResult, ErrorEnvelope>,
{
    fn run(&self, context: &HookContext) -> Result<HookResult, ErrorEnvelope> {
        self(context)
    }
}

struct RegisteredHook {
    name: String,
    handler: Box<dyn Hook>,
}

/// Hook 注册表。required 生命周期点在缺席、拒绝或执行失败时一律 fail-closed。
pub struct HookRegistry {
    hooks: BTreeMap<HookPoint, Vec<RegisteredHook>>,
    required: BTreeSet<HookPoint>,
    running: Cell<bool>,
    next_call_id: Cell<u64>,
}

impl HookRegistry {
    pub fn new(required: impl IntoIterator<Item = HookPoint>) -> Self {
        Self {
            hooks: BTreeMap::new(),
            required: required.into_iter().collect(),
            running: Cell::new(false),
            next_call_id: Cell::new(0),
        }
    }

    pub fn register(
        &mut self,
        point: HookPoint,
        name: impl Into<String>,
        handler: impl Hook + 'static,
    ) -> Result<(), ErrorEnvelope> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "hook name must not be blank",
            ));
        }
        let hooks = self.hooks.entry(point).or_default();
        if hooks.iter().any(|hook| hook.name == name) {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                format!("duplicate {} hook: {name}", point.as_str()),
            ));
        }
        hooks.push(RegisteredHook {
            name,
            handler: Box::new(handler),
        });
        Ok(())
    }

    pub(crate) fn execute(
        &self,
        context: &HookContext,
        session: &mut dyn SessionStore,
    ) -> Result<(), ErrorEnvelope> {
        if self.running.replace(true) {
            return Err(ErrorEnvelope::new(
                ErrorCode::Internal,
                "recursive hook execution rejected",
            ));
        }
        let result = self.execute_inner(context, session);
        self.running.set(false);
        result
    }

    fn execute_inner(
        &self,
        context: &HookContext,
        session: &mut dyn SessionStore,
    ) -> Result<(), ErrorEnvelope> {
        let required = self.required.contains(&context.point);
        let Some(hooks) = self.hooks.get(&context.point) else {
            return if required {
                let error = ErrorEnvelope::new(
                    ErrorCode::ApprovalRejected,
                    format!("required {} hook is missing", context.point.as_str()),
                );
                self.append_missing(context, session, &error)?;
                Err(error)
            } else {
                Ok(())
            };
        };

        for hook in hooks {
            let call_id = self.call_id(context.point, &hook.name);
            append(
                session,
                Event::ToolCallRequested {
                    call_id: call_id.clone(),
                    tool: format!("hook:{}:{}", context.point.as_str(), hook.name),
                    args: context.event_args(),
                },
            )?;
            let decision = hook.handler.run(context);
            let (outcome, failure) = project_decision(decision);
            append(session, Event::ToolResultAdded { call_id, outcome })?;
            if required {
                if let Some(error) = failure {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn append_missing(
        &self,
        context: &HookContext,
        session: &mut dyn SessionStore,
        error: &ErrorEnvelope,
    ) -> Result<(), ErrorEnvelope> {
        let call_id = self.call_id(context.point, "missing");
        append(
            session,
            Event::ToolCallRequested {
                call_id: call_id.clone(),
                tool: format!("hook:{}:missing", context.point.as_str()),
                args: context.event_args(),
            },
        )?;
        append(
            session,
            Event::ToolResultAdded {
                call_id,
                outcome: ToolOutcome::Failure {
                    error: error.clone(),
                },
            },
        )
    }

    fn call_id(&self, point: HookPoint, name: &str) -> String {
        let sequence = self.next_call_id.get();
        self.next_call_id.set(sequence.saturating_add(1));
        format!("hook:{}:{name}:{sequence}", point.as_str())
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new([])
    }
}

fn project_decision(
    decision: Result<HookResult, ErrorEnvelope>,
) -> (ToolOutcome, Option<ErrorEnvelope>) {
    match decision {
        Ok(result) if result.allowed => (
            ToolOutcome::Success {
                value: serde_json::json!({"allowed": true, "note": result.note}),
            },
            None,
        ),
        Ok(result) => {
            let error = ErrorEnvelope::new(ErrorCode::ApprovalRejected, result.note);
            (
                ToolOutcome::Failure {
                    error: error.clone(),
                },
                Some(error),
            )
        }
        Err(error) => (
            ToolOutcome::Failure {
                error: error.clone(),
            },
            Some(error),
        ),
    }
}

fn append(session: &mut dyn SessionStore, event: Event) -> Result<(), ErrorEnvelope> {
    session.append(event).map(|_| ()).map_err(|error| {
        ErrorEnvelope::new(
            ErrorCode::Internal,
            format!("failed to append hook audit event: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SequencedEvent;
    use std::path::{Path, PathBuf};

    struct MemorySession {
        events: Vec<SequencedEvent>,
        path: PathBuf,
    }

    impl SessionStore for MemorySession {
        fn append(&mut self, event: Event) -> std::io::Result<SequencedEvent> {
            let event = SequencedEvent {
                seq: self.events.len() as u64,
                event,
            };
            self.events.push(event.clone());
            Ok(event)
        }

        fn len(&self) -> u64 {
            self.events.len() as u64
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn replay_events(&self) -> std::io::Result<Vec<SequencedEvent>> {
            Ok(self.events.clone())
        }
    }

    #[test]
    fn recursive_execution_is_rejected_without_emitting_nested_events() {
        let hooks = HookRegistry::default();
        let mut session = MemorySession {
            events: Vec::new(),
            path: PathBuf::from("memory://hooks"),
        };
        hooks.running.set(true);
        let error = hooks
            .execute(
                &HookContext::turn(HookPoint::TurnFailed, 1, None),
                &mut session,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(session.events.is_empty());
    }
}
