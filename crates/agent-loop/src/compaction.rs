//! P3/TASK-302：把 context 压缩计划应用到模型可见历史并留事件。

use crate::AgentLoop;
use context::{CompactionEntry, CompactionKind, SummaryProvider, TwoStageCompactor};
use model_provider::ChatMessage;
use protocol::{ErrorCode, ErrorEnvelope, Event};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCompaction {
    pub compacted_messages: usize,
    pub pruned_tool_results: usize,
    pub summary: String,
}

#[derive(Clone, Copy)]
pub struct OverflowRecovery<'a> {
    pub(crate) compactor: &'a TwoStageCompactor,
    pub(crate) summarizer: &'a dyn SummaryProvider,
    pub(crate) max_retries: u32,
}

impl<'a> OverflowRecovery<'a> {
    pub fn new(
        compactor: &'a TwoStageCompactor,
        summarizer: &'a dyn SummaryProvider,
        max_retries: u32,
    ) -> Result<Self, &'static str> {
        if max_retries == 0 {
            return Err("overflow recovery requires at least one retry");
        }
        Ok(Self {
            compactor,
            summarizer,
            max_retries,
        })
    }
}

impl AgentLoop<'_> {
    /// 两段式压缩模型历史；没有安全前缀时不修改历史，也不写伪事件。
    pub fn compact_chat_history(
        &mut self,
        compactor: &TwoStageCompactor,
        summarizer: &dyn SummaryProvider,
    ) -> Result<Option<HistoryCompaction>, ErrorEnvelope> {
        compact_history(self.session, &mut self.chat_history, compactor, summarizer)
    }
}

pub(crate) fn compact_history(
    session: &mut session::JsonlSession,
    history: &mut Vec<ChatMessage>,
    compactor: &TwoStageCompactor,
    summarizer: &dyn SummaryProvider,
) -> Result<Option<HistoryCompaction>, ErrorEnvelope> {
    let entries: Vec<_> = history.iter().map(to_compaction_entry).collect();
    let Some(plan) = compactor.plan(&entries, summarizer)? else {
        return Ok(None);
    };
    session
        .append(Event::CompactionApplied {
            summary: plan.summary.clone(),
        })
        .map_err(|error| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("failed to append compaction event: {error}"),
            )
        })?;

    for (index, entry) in plan.entries_after_pruning.iter().enumerate() {
        if matches!(entry.kind, CompactionKind::ToolResult(_)) {
            history[index].content.clone_from(&entry.text);
        }
    }
    history.drain(..plan.compacted_prefix);
    history.insert(
        0,
        ChatMessage::system(format!("Compacted conversation summary:\n{}", plan.summary)),
    );
    Ok(Some(HistoryCompaction {
        compacted_messages: plan.compacted_prefix,
        pruned_tool_results: plan.pruned_tool_results,
        summary: plan.summary,
    }))
}

fn to_compaction_entry(message: &ChatMessage) -> CompactionEntry {
    let kind = if let Some(calls) = &message.tool_calls {
        CompactionKind::ToolCalls(calls.iter().map(|call| call.id.clone()).collect())
    } else if let Some(call_id) = &message.tool_call_id {
        CompactionKind::ToolResult(call_id.clone())
    } else {
        CompactionKind::Message
    };
    CompactionEntry {
        kind,
        text: message.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentLoop, ModelProvider};
    use context::ToolResultPruner;
    use model_provider::{ChatModel, ChatReply, ToolCallRequest};
    use protocol::ModelCallSpec;
    use session::{replay, JsonlSession};
    use std::cell::Cell;
    use std::path::PathBuf;
    use tools::ToolRegistry;

    struct Unused;
    impl ModelProvider for Unused {
        fn complete(&self, _: &str) -> Result<String, ErrorEnvelope> {
            unreachable!()
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-compact-{}-{name}", std::process::id()))
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("old question"),
            ChatMessage::assistant_with_tool_calls(vec![ToolCallRequest {
                id: "c1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }]),
            ChatMessage::tool_result("c1", "0123456789"),
            ChatMessage::assistant("old answer"),
            ChatMessage::user("recent question"),
        ]
    }

    #[test]
    fn applies_summary_and_records_compaction_event() {
        let path = tmp("success.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut agent = AgentLoop::new(&mut session, &tools, &Unused);
        agent.chat_history = history();
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(5, 3).unwrap());
        let result = agent
            .compact_chat_history(&compactor, &|input: &str| {
                assert!(input.contains("012\n[tool result pruned: 10 chars]"));
                Ok("summary".into())
            })
            .unwrap()
            .unwrap();
        assert_eq!(result.compacted_messages, 4);
        assert_eq!(result.pruned_tool_results, 1);
        assert_eq!(agent.chat_history.len(), 2);
        assert_eq!(
            agent.chat_history[0],
            ChatMessage::system("Compacted conversation summary:\nsummary")
        );
        assert_eq!(agent.chat_history[1], ChatMessage::user("recent question"));
        assert!(matches!(
            replay(&path).unwrap().last().unwrap().event,
            Event::CompactionApplied { .. }
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn summary_failure_leaves_history_and_event_stream_unchanged() {
        let path = tmp("failure.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let mut agent = AgentLoop::new(&mut session, &tools, &Unused);
        agent.chat_history = history();
        let original = agent.chat_history.clone();
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(5, 3).unwrap());
        let error = agent
            .compact_chat_history(&compactor, &|_: &str| {
                Err(ErrorEnvelope::new(ErrorCode::ModelStreamBroken, "failed"))
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ModelStreamBroken);
        assert_eq!(agent.chat_history, original);
        assert!(replay(&path).unwrap().is_empty());
        std::fs::remove_file(path).ok();
    }

    struct OverflowThenSuccess(Cell<u32>);

    impl ChatModel for OverflowThenSuccess {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            let call = self.0.get();
            self.0.set(call + 1);
            if call == 0 {
                Err(ErrorEnvelope::new(
                    ErrorCode::ContextWindowExceeded,
                    "overflow",
                ))
            } else {
                Ok(ChatReply {
                    text: "recovered".into(),
                    finish_reason: Some("stop".into()),
                    tool_calls: Vec::new(),
                })
            }
        }
    }

    struct AlwaysOverflow(Cell<u32>);

    impl ChatModel for AlwaysOverflow {
        fn stream_chat(
            &self,
            _: &ModelCallSpec,
            _: &[ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<ChatReply, ErrorEnvelope> {
            self.0.set(self.0.get() + 1);
            Err(ErrorEnvelope::new(
                ErrorCode::ContextWindowExceeded,
                "overflow",
            ))
        }
    }

    fn spec() -> ModelCallSpec {
        ModelCallSpec {
            model: "mock".into(),
            base_url: "http://127.0.0.1".into(),
            temperature: None,
        }
    }

    #[test]
    fn overflow_compacts_and_retries_without_duplicate_user_event() {
        let path = tmp("overflow-retry.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let model = OverflowThenSuccess(Cell::new(0));
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(20, 10).unwrap());
        let summarizer = |_: &str| Ok("older context".into());
        let recovery = OverflowRecovery::new(&compactor, &summarizer, 1).unwrap();
        let mut agent = AgentLoop::with_chat(&mut session, &tools, &model, spec());
        agent.chat_history = vec![ChatMessage::user("old"), ChatMessage::assistant("answer")];
        agent.overflow_recovery = Some(recovery);
        agent.inbox.push("new");

        assert_eq!(agent.run_turn(), 1);
        assert_eq!(model.0.get(), 2);
        let events = replay(&path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|entry| matches!(entry.event, Event::UserMessage { .. }))
                .count(),
            1
        );
        assert!(events
            .iter()
            .any(|entry| matches!(entry.event, Event::CompactionApplied { .. })));
        assert!(matches!(
            events.last().unwrap().event,
            Event::TurnCompleted { .. }
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn exhausted_overflow_retry_aborts_after_audited_compaction() {
        let path = tmp("overflow-exhausted.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let tools = ToolRegistry::default();
        let model = AlwaysOverflow(Cell::new(0));
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(20, 10).unwrap());
        let summarizer = |_: &str| Ok("older context".into());
        assert!(OverflowRecovery::new(&compactor, &summarizer, 0).is_err());
        let recovery = OverflowRecovery::new(&compactor, &summarizer, 1).unwrap();
        let mut agent = AgentLoop::with_chat(&mut session, &tools, &model, spec());
        agent.chat_history = vec![ChatMessage::user("old"), ChatMessage::assistant("answer")];
        agent.overflow_recovery = Some(recovery);
        agent.inbox.push("new");

        assert_eq!(agent.run_turn(), 0);
        assert_eq!(model.0.get(), 2);
        let events = replay(&path).unwrap();
        assert!(events
            .iter()
            .any(|entry| matches!(entry.event, Event::CompactionApplied { .. })));
        assert!(matches!(
            events.last().unwrap().event,
            Event::TurnAborted { .. }
        ));
        std::fs::remove_file(path).ok();
    }
}
