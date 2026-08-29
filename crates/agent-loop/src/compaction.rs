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

impl AgentLoop<'_> {
    /// 两段式压缩模型历史；没有安全前缀时不修改历史，也不写伪事件。
    pub fn compact_chat_history(
        &mut self,
        compactor: &TwoStageCompactor,
        summarizer: &dyn SummaryProvider,
    ) -> Result<Option<HistoryCompaction>, ErrorEnvelope> {
        let entries: Vec<_> = self.chat_history.iter().map(to_compaction_entry).collect();
        let Some(plan) = compactor.plan(&entries, summarizer)? else {
            return Ok(None);
        };
        self.session
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
                self.chat_history[index].content.clone_from(&entry.text);
            }
        }
        self.chat_history.drain(..plan.compacted_prefix);
        self.chat_history.insert(
            0,
            ChatMessage::system(format!("Compacted conversation summary:\n{}", plan.summary)),
        );
        Ok(Some(HistoryCompaction {
            compacted_messages: plan.compacted_prefix,
            pruned_tool_results: plan.pruned_tool_results,
            summary: plan.summary,
        }))
    }
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
    use model_provider::ToolCallRequest;
    use session::{replay, JsonlSession};
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
}
