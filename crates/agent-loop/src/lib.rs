//! P3：显式状态机 + Inbox 唤醒的 Agent 主循环（同步骨架）。
//! 生产版把执行器换成 tokio 不改协议：事件流即契约。

use protocol::{ErrorCode, Event};
use session::JsonlSession;
use tools::ToolRegistry;

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
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, text: impl Into<String>) {
        self.messages.push(text.into());
    }
    pub fn drain(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }
}

/// 模型提供者抽象：测试注入 mock 与故障（P6）。
pub trait ModelProvider {
    fn complete(&self, user_text: &str) -> Result<String, protocol::ErrorEnvelope>;
}

pub struct AgentLoop<'a> {
    pub phase: Phase,
    pub inbox: Inbox,
    pub session: &'a mut JsonlSession,
    pub tools: &'a ToolRegistry,
    pub model: &'a dyn ModelProvider,
}

impl<'a> AgentLoop<'a> {
    pub fn new(
        session: &'a mut JsonlSession,
        tools: &'a ToolRegistry,
        model: &'a dyn ModelProvider,
    ) -> Self {
        Self {
            phase: Phase::Idle,
            inbox: Inbox::new(),
            session,
            tools,
            model,
        }
    }

    /// 一个 turn：drain inbox → 逐条采样 → 终结事件。
    /// 错误按 code 路由：窗口超限留给 context 触发强制压缩后重试；
    /// 其余错误中止 turn 并留痕——绝不静默。
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
            self.session.append(Event::UserMessage { text }).ok();
            match self.model.complete("") {
                Ok(reply) => {
                    self.session
                        .append(Event::AssistantMessage { text: reply })
                        .ok();
                    completed += 1;
                }
                Err(e) if e.code == ErrorCode::ContextWindowExceeded => {
                    // TODO(context): 接入强制压缩后自动重试（P4 双触发之二）
                    self.abort(turn_id, e.message);
                    return completed;
                }
                Err(e) => {
                    self.abort(turn_id, e.message);
                    return completed;
                }
            }
        }

        self.session.append(Event::TurnCompleted { turn_id }).ok();
        self.phase = Phase::Idle;
        completed
    }

    fn abort(&mut self, turn_id: u64, reason: String) {
        self.session
            .append(Event::TurnAborted { turn_id, reason })
            .ok();
        self.phase = Phase::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ErrorEnvelope, SequencedEvent};
    use session::JsonlSession;
    use std::path::PathBuf;

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

    fn events(path: &PathBuf) -> Vec<SequencedEvent> {
        session::replay(path).unwrap()
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

        // started / user / assistant / completed
        let evs = events(&path);
        assert_eq!(evs.len(), 4);
        assert_eq!(
            evs.last().unwrap().event,
            Event::TurnCompleted { turn_id: 0 }
        );
        std::fs::remove_file(&path).ok();
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
}
