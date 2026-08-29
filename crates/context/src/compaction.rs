//! P3/TASK-302：工具结果裁剪后再摘要替换安全前缀。

use protocol::{ErrorCode, ErrorEnvelope};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionKind {
    Message,
    ToolCalls(Vec<String>),
    ToolResult(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionEntry {
    pub kind: CompactionKind,
    pub text: String,
}

pub trait SummaryProvider {
    fn summarize(&self, input: &str) -> Result<String, ErrorEnvelope>;
}

impl<F> SummaryProvider for F
where
    F: Fn(&str) -> Result<String, ErrorEnvelope>,
{
    fn summarize(&self, input: &str) -> Result<String, ErrorEnvelope> {
        self(input)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultPruner {
    max_chars: usize,
    preview_chars: usize,
}

impl ToolResultPruner {
    pub fn new(max_chars: usize, preview_chars: usize) -> Result<Self, &'static str> {
        if max_chars == 0 || preview_chars > max_chars {
            return Err("tool result limits require max_chars > 0 and preview_chars <= max_chars");
        }
        Ok(Self {
            max_chars,
            preview_chars,
        })
    }

    pub fn prune(&self, entries: &mut [CompactionEntry]) -> usize {
        let mut pruned = 0;
        for entry in entries {
            if !matches!(entry.kind, CompactionKind::ToolResult(_))
                || entry.text.chars().count() <= self.max_chars
            {
                continue;
            }
            let original_chars = entry.text.chars().count();
            let preview: String = entry.text.chars().take(self.preview_chars).collect();
            entry.text = format!("{preview}\n[tool result pruned: {original_chars} chars]");
            pruned += 1;
        }
        pruned
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub compacted_prefix: usize,
    pub summary: String,
    pub entries_after_pruning: Vec<CompactionEntry>,
    pub pruned_tool_results: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwoStageCompactor {
    retain_recent: usize,
    pruner: ToolResultPruner,
}

impl TwoStageCompactor {
    pub fn new(retain_recent: usize, pruner: ToolResultPruner) -> Self {
        Self {
            retain_recent,
            pruner,
        }
    }

    pub fn plan(
        &self,
        entries: &[CompactionEntry],
        summarizer: &dyn SummaryProvider,
    ) -> Result<Option<CompactionPlan>, ErrorEnvelope> {
        let Some(cut) = safe_prefix_len(entries, self.retain_recent) else {
            return Ok(None);
        };
        let mut pruned_entries = entries.to_vec();
        let pruned_tool_results = self.pruner.prune(&mut pruned_entries);
        let input = render_for_summary(&pruned_entries[..cut]);
        let summary = summarizer.summarize(&input)?;
        if summary.trim().is_empty() {
            return Err(ErrorEnvelope::new(
                ErrorCode::Internal,
                "compaction summarizer returned an empty summary",
            ));
        }
        Ok(Some(CompactionPlan {
            compacted_prefix: cut,
            summary,
            entries_after_pruning: pruned_entries,
            pruned_tool_results,
        }))
    }
}

/// 选择尽量大的安全前缀，同时至少保留 `retain_recent` 条。
pub fn safe_prefix_len(entries: &[CompactionEntry], retain_recent: usize) -> Option<usize> {
    let desired = entries.len().saturating_sub(retain_recent);
    (1..=desired)
        .rev()
        .find(|cut| pair_complete_at_cut(entries, *cut))
}

fn pair_complete_at_cut(entries: &[CompactionEntry], cut: usize) -> bool {
    let mut positions: BTreeMap<&str, (Option<usize>, Option<usize>)> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        match &entry.kind {
            CompactionKind::ToolCalls(ids) => {
                for id in ids {
                    let position = positions.entry(id).or_default();
                    if position.0.replace(index).is_some() {
                        return false;
                    }
                }
            }
            CompactionKind::ToolResult(id) => {
                let position = positions.entry(id).or_default();
                if position.1.replace(index).is_some() {
                    return false;
                }
            }
            CompactionKind::Message => {}
        }
    }
    positions
        .values()
        .all(|(call, result)| match (call, result) {
            (Some(call), Some(result)) => (*call < cut) == (*result < cut),
            (Some(call), None) => *call >= cut,
            (None, Some(result)) => *result >= cut,
            (None, None) => true,
        })
}

fn render_for_summary(entries: &[CompactionEntry]) -> String {
    entries
        .iter()
        .map(|entry| match &entry.kind {
            CompactionKind::Message => format!("message: {}", entry.text),
            CompactionKind::ToolCalls(ids) => {
                format!("tool_calls({}): {}", ids.join(","), entry.text)
            }
            CompactionKind::ToolResult(id) => format!("tool_result({id}): {}", entry.text),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> CompactionEntry {
        CompactionEntry {
            kind: CompactionKind::Message,
            text: text.into(),
        }
    }

    fn call(id: &str) -> CompactionEntry {
        CompactionEntry {
            kind: CompactionKind::ToolCalls(vec![id.into()]),
            text: "call".into(),
        }
    }

    fn result(id: &str, text: &str) -> CompactionEntry {
        CompactionEntry {
            kind: CompactionKind::ToolResult(id.into()),
            text: text.into(),
        }
    }

    #[test]
    fn two_stages_prune_then_summarize_without_splitting_pair() {
        let entries = vec![
            message("old"),
            call("c1"),
            result("c1", "abcdefghij"),
            message("recent"),
        ];
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(5, 3).unwrap());
        let plan = compactor
            .plan(&entries, &|input: &str| {
                assert!(input.contains("abc\n[tool result pruned: 10 chars]"));
                Ok("old summary".into())
            })
            .unwrap()
            .unwrap();
        assert_eq!(plan.compacted_prefix, 3);
        assert_eq!(plan.pruned_tool_results, 1);
        assert_eq!(plan.summary, "old summary");
    }

    #[test]
    fn summarizer_failure_and_empty_summary_fail_explicitly() {
        let entries = vec![message("old"), message("new")];
        let compactor = TwoStageCompactor::new(1, ToolResultPruner::new(10, 5).unwrap());
        let error = compactor
            .plan(&entries, &|_: &str| {
                Err(ErrorEnvelope::new(ErrorCode::ModelStreamBroken, "cut"))
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ModelStreamBroken);
        let error = compactor
            .plan(&entries, &|_: &str| Ok("  ".into()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[test]
    fn random_safe_prefix_property_never_splits_tool_pairs() {
        let mut state = 0x5eed_u64;
        for case in 0..500 {
            let pairs = (next(&mut state) % 8 + 1) as usize;
            let mut entries = vec![message("head")];
            for pair in 0..pairs {
                if next(&mut state).is_multiple_of(2) {
                    entries.push(message("between"));
                }
                let id = format!("c{case}-{pair}");
                entries.push(call(&id));
                if next(&mut state).is_multiple_of(2) {
                    entries.push(message("inside"));
                }
                entries.push(result(&id, "result"));
            }
            entries.push(message("tail"));
            let retain = (next(&mut state) as usize) % entries.len();
            if let Some(cut) = safe_prefix_len(&entries, retain) {
                assert!(pair_complete_at_cut(&entries, cut));
            }
        }
    }

    #[test]
    fn no_safe_nonempty_prefix_returns_none() {
        let entries = vec![call("open"), message("tail")];
        assert_eq!(safe_prefix_len(&entries, 0), None);
        assert!(ToolResultPruner::new(0, 0).is_err());
        assert!(ToolResultPruner::new(3, 4).is_err());
    }

    fn next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *state
    }
}
