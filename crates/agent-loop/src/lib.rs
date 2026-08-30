//! P3：显式状态机 + Inbox 唤醒的 Agent 主循环（同步骨架）。
//! 生产版把执行器换成 tokio 不改协议：事件流即契约。

mod compaction;
mod hooks;
#[cfg(test)]
mod hooks_subagent_tests;
#[cfg(test)]
mod hooks_tests;
mod mcp_bridge;
mod role_config;
mod subagent;
mod subagent_lifecycle;
mod subagent_policy;

pub use compaction::{HistoryCompaction, OverflowRecovery};
pub use hooks::{Hook, HookContext, HookPoint, HookRegistry, HookResult};
pub use mcp_bridge::McpInvocation;
pub use role_config::{
    parse_roles, AgentRole, RoleCatalog, RoleSubtask, RoleTaskBudget, RoleTaskIdentity,
};
pub use subagent::{SubagentReport, SubagentRunner, SubagentTask, SubagentTrace};
pub use subagent_lifecycle::{SubagentCancellation, SubagentDelegation};
pub use subagent_policy::{SubagentPolicy, SubagentRequest};

use context::{BudgetLedger, TokenMeter, TokenSource};
use protocol::{ErrorCode, ErrorEnvelope, Event, ModelToolCall, TokenUsageSource, ToolOutcome};
use session::SessionStore;
use std::collections::BTreeSet;
use tools::{ToolAudit, ToolExecution, ToolRegistry};

use model_provider::ChatMessage;
use model_provider::ChatModel;
use protocol::ModelCallSpec;

/// 单活跃 turn 的显式状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Maintenance,
}

/// 收件箱：消息驱动唤醒的唯一入口。
#[derive(Default)]
pub struct Inbox {
    messages: Vec<String>,
    boundary_reports: Vec<String>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, text: impl Into<String>) {
        self.messages.push(text.into());
    }
    pub fn drain(&mut self) -> Vec<String> {
        self.messages.append(&mut self.boundary_reports);
        std::mem::take(&mut self.messages)
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    fn queue_boundary_report(&mut self, text: String) {
        self.boundary_reports.push(text);
    }
}

/// 模型提供者抽象：测试注入 mock 与故障（P6）。
pub trait ModelProvider {
    fn complete(&self, user_text: &str) -> Result<String, protocol::ErrorEnvelope>;
}

/// chat 路径启用时占位的 legacy Provider：绝不应被调度（fail-fast 暴露误用）。
struct ChatPathActive;

impl ModelProvider for ChatPathActive {
    fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
        Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            "chat 路径已启用；legacy ModelProvider 不应被调用",
        ))
    }
}

static CHAT_PATH_ACTIVE: ChatPathActive = ChatPathActive;

/// 一次 chat 工具闭环 turn 的采样配置（聚拢参数，避免超长参数列表）。
struct ChatTurnConfig<'a> {
    spec: &'a ModelCallSpec,
    tool_definitions: Option<&'a serde_json::Value>,
    max_tool_rounds: u32,
    external_events: Option<&'a dyn Fn() -> Vec<Event>>,
    overflow_recovery: Option<OverflowRecovery<'a>>,
    hooks: Option<&'a HookRegistry>,
    agent_path: &'a [String],
}

pub struct AgentLoop<'a> {
    pub phase: Phase,
    pub inbox: Inbox,
    pub session: &'a mut dyn SessionStore,
    pub tools: &'a ToolRegistry,
    pub model: &'a dyn ModelProvider,
    /// TASK-103：真实模型路径（工具调用闭环）。与 `call_spec` 同时就位时启用。
    pub chat: Option<&'a dyn ChatModel>,
    /// 一次采样的调用规格（模型名 / base_url / 温度）。
    pub call_spec: Option<ModelCallSpec>,
    /// 广告给模型的 tools 数组（OpenAI 格式原始 JSON；None = 不广告工具）。
    pub tool_definitions: Option<serde_json::Value>,
    /// 单个用户消息内允许的最大采样轮次（防工具调用死循环）。
    pub max_tool_rounds: u32,
    /// 多轮对话记忆（TASK-104）：跨 turn 累积、模型可见的消息历史。
    /// 会话重开时可由事件流重建后注入。
    pub chat_history: Vec<ChatMessage>,
    /// 代理等进程边界组件产生的事件，由主循环在模型调用后按序吸收入会话。
    pub external_events: Option<&'a dyn Fn() -> Vec<Event>>,
    /// TASK-303：窗口溢出时的强制压缩与有限自动重试配置。
    pub overflow_recovery: Option<OverflowRecovery<'a>>,
    /// TASK-503：同步生命周期 Hook；注册表本身不持有 session 写能力。
    pub hooks: Option<&'a HookRegistry>,
    /// TASK-602：从根到当前代理的稳定身份路径；usage 通过它归集到所有祖先。
    agent_path: Vec<String>,
}

impl<'a> AgentLoop<'a> {
    pub fn new(
        session: &'a mut dyn SessionStore,
        tools: &'a ToolRegistry,
        model: &'a dyn ModelProvider,
    ) -> Self {
        Self {
            phase: Phase::Idle,
            inbox: Inbox::new(),
            session,
            tools,
            model,
            chat: None,
            call_spec: None,
            tool_definitions: None,
            max_tool_rounds: 8,
            chat_history: Vec::new(),
            external_events: None,
            overflow_recovery: None,
            hooks: None,
            agent_path: vec!["root".into()],
        }
    }

    /// TASK-103：接入真实模型（工具调用闭环路径）。
    pub fn with_chat(
        session: &'a mut dyn SessionStore,
        tools: &'a ToolRegistry,
        chat: &'a dyn ChatModel,
        call_spec: ModelCallSpec,
    ) -> Self {
        Self {
            phase: Phase::Idle,
            inbox: Inbox::new(),
            session,
            tools,
            model: &CHAT_PATH_ACTIVE,
            chat: Some(chat),
            call_spec: Some(call_spec),
            tool_definitions: None,
            max_tool_rounds: 8,
            chat_history: Vec::new(),
            external_events: None,
            overflow_recovery: None,
            hooks: None,
            agent_path: vec!["root".into()],
        }
    }

    /// 配置或幂等恢复根预算。已有会话中的预算不可被改写。
    pub fn configure_root_token_budget(
        &mut self,
        root_agent_id: impl Into<String>,
        token_budget: u64,
    ) -> Result<(), ErrorEnvelope> {
        let root_agent_id = root_agent_id.into();
        let events = self.session.replay_events().map_err(session_replay_error)?;
        let ledger = BudgetLedger::replay(&events)?;
        let candidate = Event::TokenBudgetConfigured {
            root_agent_id: root_agent_id.clone(),
            token_budget,
        };
        let mut checked = ledger.clone();
        checked.apply(&candidate)?;
        if ledger.token_budget().is_none() {
            self.session
                .append(candidate)
                .map_err(session_append_error)?;
        }
        self.agent_path = vec![root_agent_id];
        Ok(())
    }

    /// 子代理显式继承完整 lineage；空白、环或与已配置根不一致均拒绝。
    pub fn set_agent_path(&mut self, agent_path: Vec<String>) -> Result<(), ErrorEnvelope> {
        validate_agent_path(&agent_path)?;
        let events = self.session.replay_events().map_err(session_replay_error)?;
        let ledger = BudgetLedger::replay(&events)?;
        if ledger
            .root_agent_id()
            .is_some_and(|root| root != agent_path[0])
        {
            return Err(ErrorEnvelope::new(
                ErrorCode::Internal,
                "agent path does not inherit the configured root",
            ));
        }
        self.agent_path = agent_path;
        Ok(())
    }

    /// TASK-404：在进程内运行隔离子代理，只把最终 report 成对回传父事件流。
    pub fn run_subagent(
        &mut self,
        task: &SubagentTask,
        runner: &dyn SubagentRunner,
    ) -> Result<SubagentReport, ErrorEnvelope> {
        let policy = SubagentPolicy::local_default();
        let request = SubagentRequest::local_default();
        let cancellation = SubagentCancellation::new();
        let delegation = SubagentDelegation::new(
            &request,
            &policy,
            &policy,
            protocol::SubagentReportDelivery::Quiet,
            &cancellation,
        );
        self.run_subagent_lifecycle(task, &delegation, runner)
    }

    /// TASK-409：在 runner 前应用父/子策略与本次资源请求。
    pub fn run_subagent_with_policy(
        &mut self,
        task: &SubagentTask,
        request: &SubagentRequest,
        parent_policy: &SubagentPolicy,
        child_policy: &SubagentPolicy,
        runner: &dyn SubagentRunner,
    ) -> Result<SubagentReport, ErrorEnvelope> {
        let cancellation = SubagentCancellation::new();
        let delegation = SubagentDelegation::new(
            request,
            parent_policy,
            child_policy,
            protocol::SubagentReportDelivery::Quiet,
            &cancellation,
        );
        self.run_subagent_lifecycle(task, &delegation, runner)
    }

    /// TASK-410：运行带 lineage、取消传播和显式报告投递方式的子代理。
    pub fn run_subagent_lifecycle(
        &mut self,
        task: &SubagentTask,
        delegation: &SubagentDelegation<'_>,
        runner: &dyn SubagentRunner,
    ) -> Result<SubagentReport, ErrorEnvelope> {
        let result = subagent_lifecycle::run(self.session, task, delegation, runner);
        if let Ok(report) = &result {
            if delegation.delivery() == protocol::SubagentReportDelivery::NextStep {
                self.inbox.queue_boundary_report(report.text.clone());
            }
        }
        let outcome = match &result {
            Ok(report) => ToolOutcome::Success {
                value: serde_json::json!({
                    "task_id": report.task_id,
                    "child_event_count": report.child_event_count,
                }),
            },
            Err(error) => ToolOutcome::Failure {
                error: error.clone(),
            },
        };
        let hook_result = self.execute_hook(HookContext::subagent(task.id(), outcome));
        if result.is_ok() {
            hook_result?;
        }
        result
    }

    /// 一个 turn：drain inbox → 逐条采样 → 终结事件。
    /// 错误按 code 路由：窗口超限留给 context 触发强制压缩后重试；
    /// 其余错误中止 turn 并留痕——绝不静默。
    /// `with_chat` 就位时走工具调用闭环路径（TASK-103），否则走 legacy 演示路径。
    pub fn run_turn(&mut self) -> u64 {
        assert_ne!(
            self.phase,
            Phase::Running,
            "single-active-turn contract violated: run_turn reentered while Running"
        );
        self.phase = Phase::Running;
        let turn_id = self.session.len();
        self.session
            .append(Event::TurnStarted { turn_id })
            .expect("append turn_start");

        let mut completed = 0u64;
        for text in self.inbox.drain() {
            let result = match (self.chat, self.call_spec.clone()) {
                (Some(chat), Some(spec)) => Self::run_chat_turn(
                    self.session,
                    self.tools,
                    chat,
                    &ChatTurnConfig {
                        spec: &spec,
                        tool_definitions: self.tool_definitions.as_ref(),
                        max_tool_rounds: self.max_tool_rounds,
                        external_events: self.external_events,
                        overflow_recovery: self.overflow_recovery,
                        hooks: self.hooks,
                        agent_path: &self.agent_path,
                    },
                    &mut self.chat_history,
                    turn_id,
                    &text,
                ),
                _ => {
                    self.session
                        .append(Event::UserMessage { text: text.clone() })
                        .ok();
                    let result = ensure_budget_allows_sample(self.session)
                        .and_then(|_| self.model.complete(&text));
                    result.and_then(|reply| {
                        record_usage(
                            self.session,
                            &self.agent_path,
                            format!("turn-{turn_id}-legacy"),
                            None,
                            &[text.as_str(), reply.as_str()],
                        )?;
                        self.session
                            .append(Event::AssistantMessage { text: reply })
                            .map(|_| ())
                            .map_err(session_append_error)
                    })
                }
            };
            match result {
                Ok(()) => completed += 1,
                Err(e) => {
                    let reason = self
                        .execute_hook(HookContext::turn(
                            HookPoint::TurnFailed,
                            turn_id,
                            Some(e.message.clone()),
                        ))
                        .err()
                        .unwrap_or(e)
                        .message;
                    self.abort(turn_id, reason);
                    return completed;
                }
            }
        }

        if let Err(error) =
            self.execute_hook(HookContext::turn(HookPoint::TurnCompleted, turn_id, None))
        {
            self.abort(turn_id, error.message);
            return completed;
        }
        self.session.append(Event::TurnCompleted { turn_id }).ok();
        self.phase = Phase::Idle;
        completed
    }

    /// 工具调用闭环（TASK-103）：采样 → 有 tool_call 则调度并回填 →
    /// 继续采样直至模型给出文本答复；超过 `max_tool_rounds` 强制终结。
    /// 一切自动行为落 Event（红线 5）：ModelChunkReceived / ToolCallRequested /
    /// ToolResultAdded / AssistantMessage，tool_call 与 result 严格配对（红线 4）。
    fn run_chat_turn(
        session: &mut dyn SessionStore,
        registry: &ToolRegistry,
        chat: &dyn ChatModel,
        cfg: &ChatTurnConfig<'_>,
        history: &mut Vec<ChatMessage>,
        turn_id: u64,
        user_text: &str,
    ) -> Result<(), ErrorEnvelope> {
        let call_id = format!("turn-{turn_id}");
        session
            .append(Event::UserMessage {
                text: user_text.to_string(),
            })
            .ok();
        history.push(ChatMessage::user(user_text));

        for round in 0..cfg.max_tool_rounds {
            let mut overflow_retries = 0;
            let reply = loop {
                ensure_budget_allows_sample(session)?;
                let reply = chat.stream_chat(cfg.spec, history, cfg.tool_definitions);
                if let Some(source) = cfg.external_events {
                    for event in source() {
                        session.append(event).ok();
                    }
                }
                match reply {
                    Err(error) if error.code == ErrorCode::ContextWindowExceeded => {
                        let Some(recovery) = cfg.overflow_recovery else {
                            return Err(error);
                        };
                        if overflow_retries >= recovery.max_retries {
                            return Err(error);
                        }
                        if compaction::compact_history(
                            session,
                            history,
                            recovery.compactor,
                            recovery.summarizer,
                        )?
                        .is_none()
                        {
                            return Err(error);
                        }
                        overflow_retries += 1;
                    }
                    other => break other?,
                }
            };

            let mut visible = history
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>();
            visible.push(reply.text.as_str());
            visible.extend(reply.tool_calls.iter().map(|call| call.arguments.as_str()));
            record_usage(
                session,
                cfg.agent_path,
                format!("turn-{turn_id}-sample-{round}"),
                reply.usage.map(|usage| usage.total_tokens),
                &visible,
            )?;

            if !reply.text.is_empty() {
                session
                    .append(Event::ModelChunkReceived {
                        call_id: call_id.clone(),
                        delta_text: reply.text.clone(),
                    })
                    .ok();
            }

            if reply.tool_calls.is_empty() {
                // 最终文本答复必须进历史：这是下一轮模型能看到本轮结论的唯一途径
                history.push(ChatMessage::assistant(reply.text.clone()));
                session
                    .append(Event::AssistantMessage {
                        text: reply.text.clone(),
                    })
                    .ok();
                return Ok(());
            }

            session
                .append(Event::ModelToolCallsRequested {
                    request_id: format!("turn-{turn_id}-round-{round}"),
                    calls: reply
                        .tool_calls
                        .iter()
                        .map(|call| ModelToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect(),
                })
                .ok();
            history.push(ChatMessage::assistant_with_tool_calls(
                reply.tool_calls.clone(),
            ));
            for (call_index, tc) in reply.tool_calls.iter().enumerate() {
                // 参数必须是合法 JSON：非法则不触发 handler，直接回自纠码
                // （配对完整：ToolCallRequested 先落盘，结果随后必达）。
                let (args, parse_err) =
                    match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                        Ok(v) => (v, None),
                        Err(e) => (serde_json::Value::String(tc.arguments.clone()), Some(e)),
                    };
                session
                    .append(Event::ToolCallRequested {
                        call_id: tc.id.clone(),
                        tool: tc.name.clone(),
                        args: args.clone(),
                    })
                    .ok();

                if let Err(error) = execute_hook(
                    cfg.hooks,
                    HookContext::tool(HookPoint::PreToolUse, Some(turn_id), &tc.id, &tc.name, None),
                    session,
                ) {
                    let outcome = ToolOutcome::Failure {
                        error: error.clone(),
                    };
                    session
                        .append(Event::ToolResultAdded {
                            call_id: tc.id.clone(),
                            outcome: outcome.clone(),
                        })
                        .ok();
                    history.push(ChatMessage::tool_result(
                        tc.id.clone(),
                        serde_json::to_string(&outcome).unwrap_or_default(),
                    ));
                    Self::close_unexecuted_model_calls(
                        session,
                        history,
                        &reply.tool_calls[call_index + 1..],
                        &error,
                    );
                    return Err(error);
                }

                let execution = if let Some(e) = parse_err {
                    ToolExecution::new(ToolOutcome::Failure {
                        error: ErrorEnvelope::new(
                            ErrorCode::ToolArgsInvalid,
                            format!("tool arguments 不是合法 JSON: {e}"),
                        ),
                    })
                } else {
                    registry
                        .dispatch_with_audit(&tc.name, &args)
                        .unwrap_or_else(|| {
                            ToolExecution::new(ToolOutcome::Failure {
                                error: ErrorEnvelope::new(
                                    ErrorCode::ToolArgsInvalid,
                                    format!("unknown tool: {}", tc.name),
                                ),
                            })
                        })
                };
                for audit in execution.audits {
                    match audit {
                        ToolAudit::ApprovalDecided { approved } => {
                            session
                                .append(Event::ApprovalDecided {
                                    call_id: tc.id.clone(),
                                    approved,
                                })
                                .ok();
                        }
                    }
                }
                let outcome = execution.outcome;
                session
                    .append(Event::ToolResultAdded {
                        call_id: tc.id.clone(),
                        outcome: outcome.clone(),
                    })
                    .ok();
                history.push(ChatMessage::tool_result(
                    tc.id.clone(),
                    serde_json::to_string(&outcome).unwrap_or_default(),
                ));
                if let Err(error) = execute_hook(
                    cfg.hooks,
                    HookContext::tool(
                        HookPoint::PostToolUse,
                        Some(turn_id),
                        &tc.id,
                        &tc.name,
                        Some(outcome),
                    ),
                    session,
                ) {
                    Self::close_unexecuted_model_calls(
                        session,
                        history,
                        &reply.tool_calls[call_index + 1..],
                        &error,
                    );
                    return Err(error);
                }
            }
        }

        Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            format!(
                "超过 max_tool_rounds={}，强制终结以防死循环",
                cfg.max_tool_rounds
            ),
        ))
    }

    fn close_unexecuted_model_calls(
        session: &mut dyn SessionStore,
        history: &mut Vec<ChatMessage>,
        remaining: &[model_provider::ToolCallRequest],
        cause: &ErrorEnvelope,
    ) {
        for call in remaining {
            let args = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
            session
                .append(Event::ToolCallRequested {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    args,
                })
                .ok();
            let outcome = ToolOutcome::Failure {
                error: ErrorEnvelope::new(
                    cause.code,
                    "tool was not executed because an earlier call in the batch failed",
                ),
            };
            session
                .append(Event::ToolResultAdded {
                    call_id: call.id.clone(),
                    outcome: outcome.clone(),
                })
                .ok();
            history.push(ChatMessage::tool_result(
                call.id.clone(),
                serde_json::to_string(&outcome).unwrap_or_default(),
            ));
        }
    }

    fn abort(&mut self, turn_id: u64, reason: String) {
        self.session
            .append(Event::TurnAborted { turn_id, reason })
            .ok();
        self.phase = Phase::Idle;
    }

    /// 在同步骨架的安全点中断当前 turn，并触发可审计的 interrupted Hook。
    pub fn interrupt_turn(
        &mut self,
        turn_id: u64,
        reason: impl Into<String>,
    ) -> Result<(), ErrorEnvelope> {
        if self.phase != Phase::Running {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "only a running turn can be interrupted",
            ));
        }
        let reason = reason.into();
        let hook_result = self.execute_hook(HookContext::turn(
            HookPoint::TurnInterrupted,
            turn_id,
            Some(reason.clone()),
        ));
        let abort_reason = hook_result
            .as_ref()
            .err()
            .map(|error| error.message.clone())
            .unwrap_or(reason);
        self.abort(turn_id, abort_reason);
        hook_result
    }

    pub(crate) fn execute_hook(&mut self, context: HookContext) -> Result<(), ErrorEnvelope> {
        execute_hook(self.hooks, context, self.session)
    }
}

fn ensure_budget_allows_sample(session: &dyn SessionStore) -> Result<(), ErrorEnvelope> {
    let events = session.replay_events().map_err(session_replay_error)?;
    BudgetLedger::replay(&events)?.ensure_can_sample()
}

fn record_usage(
    session: &mut dyn SessionStore,
    agent_path: &[String],
    usage_id: String,
    provider_total: Option<u64>,
    visible_segments: &[&str],
) -> Result<(), ErrorEnvelope> {
    validate_agent_path(agent_path)?;
    let measurement = TokenMeter::default().measure(provider_total, visible_segments);
    let event = Event::TokenUsageRecorded {
        usage_id,
        agent_path: agent_path.to_vec(),
        total_tokens: measurement.usage.total,
        source: match measurement.source {
            TokenSource::ProviderUsage => TokenUsageSource::Provider,
            TokenSource::Heuristic => TokenUsageSource::Heuristic,
        },
    };
    let events = session.replay_events().map_err(session_replay_error)?;
    let mut ledger = BudgetLedger::replay(&events)?;
    ledger.apply(&event)?;
    session
        .append(event)
        .map(|_| ())
        .map_err(session_append_error)
}

fn validate_agent_path(agent_path: &[String]) -> Result<(), ErrorEnvelope> {
    let mut seen = BTreeSet::new();
    if agent_path.is_empty()
        || agent_path
            .iter()
            .any(|agent| agent.trim().is_empty() || !seen.insert(agent))
    {
        return Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            "agent path must be non-empty, non-blank and acyclic",
        ));
    }
    Ok(())
}

fn session_replay_error(error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to replay token budget ledger: {error}"),
    )
}

fn session_append_error(error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to append token budget event: {error}"),
    )
}

fn execute_hook(
    hooks: Option<&HookRegistry>,
    context: HookContext,
    session: &mut dyn SessionStore,
) -> Result<(), ErrorEnvelope> {
    match hooks {
        Some(hooks) => hooks.execute(&context, session),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_provider::{ChatReply, ToolCallRequest};
    use protocol::{ErrorEnvelope, SequencedEvent};
    use session::JsonlSession;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Echo;
    impl ModelProvider for Echo {
        fn complete(&self, user_text: &str) -> Result<String, ErrorEnvelope> {
            Ok(format!("echo:{user_text}"))
        }
    }

    struct Broken;
    impl ModelProvider for Broken {
        fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
            Err(ErrorEnvelope::new(
                ErrorCode::ModelStreamBroken,
                "stream cut",
            ))
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-loop-{}-{name}", std::process::id()))
    }

    fn events(path: &Path) -> Vec<SequencedEvent> {
        session::replay(path).unwrap()
    }

    struct MemorySession {
        path: PathBuf,
        events: Vec<SequencedEvent>,
    }

    impl MemorySession {
        fn new() -> Self {
            Self {
                path: PathBuf::from("memory://agent-loop"),
                events: Vec::new(),
            }
        }
    }

    impl SessionStore for MemorySession {
        fn append(&mut self, event: Event) -> std::io::Result<SequencedEvent> {
            let sequenced = SequencedEvent {
                seq: self.events.len() as u64,
                event,
            };
            self.events.push(sequenced.clone());
            Ok(sequenced)
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
    fn happy_turn_appends_full_lifecycle() {
        let path = tmp("happy.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default();
        let mut lp = AgentLoop::new(&mut js, &reg, &Echo);
        lp.inbox.push("你好");
        assert_eq!(lp.run_turn(), 1);
        assert_eq!(lp.phase, Phase::Idle);

        // started / user / usage / assistant / completed
        let evs = events(&path);
        assert_eq!(evs.len(), 5);
        assert_eq!(
            evs.last().unwrap().event,
            Event::TurnCompleted { turn_id: 0 }
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn happy_turn_uses_the_same_loop_with_an_in_memory_store() {
        let mut memory = MemorySession::new();
        let reg = ToolRegistry::default();
        let mut lp = AgentLoop::new(&mut memory, &reg, &Echo);
        lp.inbox.push("你好");
        assert_eq!(lp.run_turn(), 1);
        assert_eq!(lp.phase, Phase::Idle);
        drop(lp);

        assert_eq!(memory.events.len(), 5);
        assert_eq!(
            memory.events.last().unwrap().event,
            Event::TurnCompleted { turn_id: 0 }
        );
    }

    #[test]
    fn model_failure_aborts_and_leaves_trace_never_silent() {
        let path = tmp("broken.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default();
        let mut lp = AgentLoop::new(&mut js, &reg, &Broken);
        lp.inbox.push("hi");
        assert_eq!(lp.run_turn(), 0);

        let evs = events(&path);
        match &evs.last().unwrap().event {
            Event::TurnAborted { reason, .. } => assert_eq!(reason, "stream cut"),
            other => panic!("expected abort event, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reentrant_run_turn_is_a_programmer_error() {
        let path = tmp("reentry.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default();
        let mut lp = AgentLoop::new(&mut js, &reg, &Echo);
        lp.phase = Phase::Running; // 模拟非法重入
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lp.run_turn()));
        assert!(result.is_err(), "单活跃 turn 契约必须显式暴露违约");
        std::fs::remove_file(&path).ok();
    }

    // ---- TASK-103：工具调用闭环（验收：三段序列 + 超轮次强制终结）----

    fn echo_spec() -> tools::ToolSpec {
        tools::ToolSpec {
            name: "echo".into(),
            description: "demo".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
            escalation_capable: false,
        }
    }

    fn chat_spec() -> ModelCallSpec {
        ModelCallSpec {
            model: "mock-model".into(),
            base_url: "http://localhost".into(),
            temperature: None,
        }
    }

    /// 脚本化 mock：按序返回预置回复（「文本→工具→文本」三段序列）。
    struct Scripted(Mutex<Vec<ChatReply>>);

    impl ChatModel for Scripted {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            let mut q = self.0.lock().unwrap();
            if q.is_empty() {
                panic!("脚本回复耗尽");
            }
            Ok(q.remove(0))
        }
    }

    /// 恒定返回工具调用的 mock：验证 max_tool_rounds 强制终结。
    struct AlwaysTool;

    impl ChatModel for AlwaysTool {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            Ok(ChatReply {
                text: String::new(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_x".into(),
                    name: "echo".into(),
                    arguments: r#"{"text":"hi"}"#.into(),
                }],
                usage: None,
            })
        }
    }

    fn tool_reply() -> ChatReply {
        ChatReply {
            text: "让我查一下".into(),
            finish_reason: Some("tool_calls".into()),
            tool_calls: vec![ToolCallRequest {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: r#"{"text":"hi"}"#.into(),
            }],
            usage: None,
        }
    }

    fn text_reply() -> ChatReply {
        ChatReply {
            text: "结果是 hi".into(),
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            usage: None,
        }
    }

    fn r2_reply() -> ChatReply {
        ChatReply {
            text: "r2".into(),
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            usage: None,
        }
    }

    /// 捕获每次采样所见消息的 mock：验证多轮记忆（TASK-104 前置）。
    struct Capturing {
        replies: Mutex<Vec<ChatReply>>,
        seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl ChatModel for Capturing {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            msgs: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            self.seen.lock().unwrap().push(msgs.to_vec());
            let mut q = self.replies.lock().unwrap();
            if q.is_empty() {
                panic!("脚本回复耗尽");
            }
            Ok(q.remove(0))
        }
    }

    fn event_kind(e: &Event) -> &'static str {
        match e {
            Event::TurnStarted { .. } => "turn_started",
            Event::UserMessage { .. } => "user_message",
            Event::AssistantMessage { .. } => "assistant_message",
            Event::ModelChunkReceived { .. } => "model_chunk",
            Event::ToolCallRequested { .. } => "tool_call_requested",
            Event::ToolResultAdded { .. } => "tool_result_added",
            Event::ModelToolCallsRequested { .. } => "model_tool_calls_requested",
            Event::CompactionApplied { .. } => "compaction",
            Event::TokenBudgetConfigured { .. } => "token_budget_configured",
            Event::TokenUsageRecorded { .. } => "token_usage_recorded",
            Event::ApprovalDecided { .. } => "approval",
            Event::NetworkAccessDenied { .. } => "network_access_denied",
            Event::SubagentStarted { .. } => "subagent_started",
            Event::SubagentCancellationRequested { .. } => "subagent_cancel_requested",
            Event::SubagentReportDelivered { .. } => "subagent_report_delivered",
            Event::SubagentStopped { .. } => "subagent_stopped",
            Event::TurnCompleted { .. } => "turn_completed",
            Event::TurnAborted { .. } => "turn_aborted",
        }
    }

    #[test]
    fn chat_loop_text_tool_text_full_sequence() {
        let path = tmp("chat-loop.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|args| ToolOutcome::Success {
                value: args["text"].clone(),
            }),
        );
        let scripted = Scripted(Mutex::new(vec![tool_reply(), text_reply()]));
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &scripted, chat_spec());
        lp.inbox.push("查一下");
        assert_eq!(lp.run_turn(), 1);
        assert_eq!(lp.phase, Phase::Idle);

        let evs = events(&path);
        let kinds: Vec<_> = evs.iter().map(|e| event_kind(&e.event)).collect();
        assert_eq!(
            kinds,
            vec![
                "turn_started",
                "user_message",
                "token_usage_recorded",
                "model_chunk", // 第 1 轮采样文本留痕
                "model_tool_calls_requested",
                "tool_call_requested",
                "tool_result_added", // 与调用严格配对
                "token_usage_recorded",
                "model_chunk",       // 第 2 轮采样文本
                "assistant_message", // 最终文本答复
                "turn_completed",
            ]
        );
        match &evs[6].event {
            Event::ToolResultAdded { outcome, .. } => match outcome {
                ToolOutcome::Success { value } => assert_eq!(value, "hi"),
                other => panic!("expected success outcome, got {other:?}"),
            },
            other => panic!("expected tool_result_added, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn exceeding_max_tool_rounds_forces_abort_and_keeps_pairing() {
        let path = tmp("chat-maxrounds.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|args| ToolOutcome::Success {
                value: args["text"].clone(),
            }),
        );
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &AlwaysTool, chat_spec());
        lp.max_tool_rounds = 3;
        lp.inbox.push("x");
        assert_eq!(lp.run_turn(), 0);
        assert_eq!(lp.phase, Phase::Idle);

        let evs = events(&path);
        match &evs.last().unwrap().event {
            Event::TurnAborted { reason, .. } => assert!(
                reason.contains("max_tool_rounds"),
                "abort 原因应说明超轮次: {reason}"
            ),
            other => panic!("expected turn_aborted, got {other:?}"),
        }
        // 配对完整：3 轮 → 3 次调用 + 3 次结果，一次不缺
        let requested = evs
            .iter()
            .filter(|e| matches!(e.event, Event::ToolCallRequested { .. }))
            .count();
        let answered = evs
            .iter()
            .filter(|e| matches!(e.event, Event::ToolResultAdded { .. }))
            .count();
        assert_eq!(requested, 3);
        assert_eq!(answered, 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_tool_yields_failure_result_and_loop_continues() {
        let path = tmp("chat-unknown.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default(); // 空注册表：任何调用都是未知工具
        let scripted = Scripted(Mutex::new(vec![
            ChatReply {
                text: String::new(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_u".into(),
                    name: "nope".into(),
                    arguments: "{}".into(),
                }],
                usage: None,
            },
            text_reply(),
        ]));
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &scripted, chat_spec());
        lp.inbox.push("hi");
        assert_eq!(lp.run_turn(), 1);

        let evs = events(&path);
        match &evs[5].event {
            Event::ToolResultAdded { outcome, .. } => match outcome {
                ToolOutcome::Failure { error } => {
                    assert_eq!(error.code, ErrorCode::ToolArgsInvalid)
                }
                other => panic!("expected failure, got {other:?}"),
            },
            other => panic!("expected tool_result_added, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn malformed_args_fail_without_invoking_handler() {
        let path = tmp("chat-badargs.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|_| panic!("handler must not run on malformed args")),
        );
        let scripted = Scripted(Mutex::new(vec![
            ChatReply {
                text: String::new(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_b".into(),
                    name: "echo".into(),
                    arguments: "{oops".into(),
                }],
                usage: None,
            },
            text_reply(),
        ]));
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &scripted, chat_spec());
        lp.inbox.push("hi");
        assert_eq!(lp.run_turn(), 1);

        let evs = events(&path);
        match &evs[5].event {
            Event::ToolResultAdded { outcome, .. } => match outcome {
                ToolOutcome::Failure { error } => {
                    assert_eq!(error.code, ErrorCode::ToolArgsInvalid)
                }
                other => panic!("expected failure, got {other:?}"),
            },
            other => panic!("expected tool_result_added, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn chat_history_spans_turns_for_multi_turn_memory() {
        let path = tmp("chat-memory.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default();
        let capturing = Capturing {
            replies: Mutex::new(vec![text_reply(), r2_reply()]),
            seen: Mutex::new(vec![]),
        };
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &capturing, chat_spec());
        lp.inbox.push("a");
        assert_eq!(lp.run_turn(), 1);
        lp.inbox.push("b");
        assert_eq!(lp.run_turn(), 1);

        let seen = capturing.seen.lock().unwrap();
        assert_eq!(seen[0].len(), 1, "首轮只有当前用户消息");
        assert_eq!(seen[1].len(), 3, "次轮必须携带首轮完整对话");
        assert_eq!(seen[1][0], ChatMessage::user("a"));
        assert_eq!(seen[1][1], ChatMessage::assistant("结果是 hi"));
        assert_eq!(seen[1][2], ChatMessage::user("b"));
        std::fs::remove_file(&path).ok();
    }

    struct UsageModel<'a>(&'a AtomicUsize);

    impl ChatModel for UsageModel<'_> {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatReply {
                text: "done".into(),
                finish_reason: Some("stop".into()),
                tool_calls: vec![],
                usage: Some(protocol::ModelTokenUsage { total_tokens: 2 }),
            })
        }
    }

    #[test]
    fn root_budget_replays_and_rejects_next_sample_before_provider_call() {
        let path = tmp("root-budget.jsonl");
        let _ = std::fs::remove_file(&path);
        let tools = ToolRegistry::default();
        let first_calls = AtomicUsize::new(0);
        {
            let mut session = JsonlSession::create(path.clone()).unwrap();
            let model = UsageModel(&first_calls);
            let mut agent = AgentLoop::with_chat(&mut session, &tools, &model, chat_spec());
            agent.configure_root_token_budget("root", 1).unwrap();
            agent.inbox.push("first");
            assert_eq!(agent.run_turn(), 1);
        }
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);

        let second_calls = AtomicUsize::new(0);
        {
            let mut resumed = JsonlSession::create(path.clone()).unwrap();
            let model = UsageModel(&second_calls);
            let mut agent = AgentLoop::with_chat(&mut resumed, &tools, &model, chat_spec());
            agent.inbox.push("second");
            assert_eq!(agent.run_turn(), 0);
        }
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
        let events = session::replay(&path).unwrap();
        let ledger = BudgetLedger::replay(&events).unwrap();
        assert_eq!(ledger.root_remaining(), Some(0));
        assert!(events.iter().any(|record| matches!(
            record.event,
            Event::TokenUsageRecorded {
                total_tokens: 2,
                source: TokenUsageSource::Provider,
                ..
            }
        )));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn audited_tool_decision_is_recorded_before_tool_result() {
        let path = tmp("chat-approval-audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let mut reg = ToolRegistry::default();
        reg.register_audited(
            echo_spec(),
            Box::new(|args| ToolExecution {
                outcome: ToolOutcome::Success {
                    value: args["text"].clone(),
                },
                audits: vec![ToolAudit::ApprovalDecided { approved: true }],
            }),
        );
        let scripted = Scripted(Mutex::new(vec![tool_reply(), text_reply()]));
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &scripted, chat_spec());
        lp.inbox.push("run");
        assert_eq!(lp.run_turn(), 1);

        let evs = events(&path);
        let approval_index = evs
            .iter()
            .position(|entry| matches!(entry.event, Event::ApprovalDecided { .. }))
            .unwrap();
        let result_index = evs
            .iter()
            .position(|entry| matches!(entry.event, Event::ToolResultAdded { .. }))
            .unwrap();
        assert!(approval_index < result_index);
        match &evs[approval_index].event {
            Event::ApprovalDecided { call_id, approved } => {
                assert_eq!(call_id, "call_1");
                assert!(*approved);
            }
            _ => unreachable!(),
        }
        std::fs::remove_file(&path).ok();
    }

    struct NetworkRejected;

    impl ChatModel for NetworkRejected {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            Err(ErrorEnvelope::new(ErrorCode::Internal, "proxy rejected"))
        }
    }

    #[test]
    fn external_proxy_event_is_recorded_before_model_failure_abort() {
        let path = tmp("network-audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let reg = ToolRegistry::default();
        let source = || {
            vec![Event::NetworkAccessDenied {
                host: "blocked.example".into(),
                port: 443,
                reason: "host_not_allowlisted".into(),
            }]
        };
        let mut lp = AgentLoop::with_chat(&mut js, &reg, &NetworkRejected, chat_spec());
        lp.external_events = Some(&source);
        lp.inbox.push("connect");
        assert_eq!(lp.run_turn(), 0);

        let evs = events(&path);
        assert!(matches!(evs[2].event, Event::NetworkAccessDenied { .. }));
        assert!(matches!(
            evs.last().unwrap().event,
            Event::TurnAborted { .. }
        ));
        std::fs::remove_file(&path).ok();
    }
}
