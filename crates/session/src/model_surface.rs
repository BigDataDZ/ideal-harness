//! D14/TASK-601：从唯一事件流派生模型可见历史，审计事件不进入上下文。

use protocol::{
    ErrorCode, ErrorEnvelope, Event, ModelSurfaceEntry, ModelSurfaceMessage, ModelToolCall,
    SequencedEvent,
};
use std::collections::{BTreeMap, BTreeSet};

/// 投影模型可见历史。任何配对、压缩来源或 replace-prefix 不变量被破坏时
/// 都 fail-closed，避免 resume 时把损坏状态继续发送给模型。
pub fn project_model_surface(
    events: &[SequencedEvent],
) -> Result<Vec<ModelSurfaceEntry>, ErrorEnvelope> {
    let mut out = Vec::new();
    let mut pending = BTreeMap::<String, u64>::new();
    let mut completed = BTreeSet::<String>::new();
    let mut declared = BTreeSet::<String>::new();
    // TASK-704：steer 输入统一延迟出账——工具批次未闭合时不能把 User 消息
    // 插进 assistant tool_calls 与 tool result 之间（provider 会拒绝该序列）。
    let mut deferred_inputs: Vec<(u64, String)> = Vec::new();

    for sequenced in events {
        match &sequenced.event {
            Event::UserInputQueued { text } => {
                deferred_inputs.push((sequenced.seq, text.clone()));
            }
            Event::UserMessage { text } => {
                require_closed(&pending, sequenced.seq)?;
                flush_deferred_inputs(&mut deferred_inputs, &mut out);
                push(
                    &mut out,
                    ModelSurfaceMessage::User { text: text.clone() },
                    sequenced.seq,
                );
            }
            Event::AssistantMessage { text } if !text.is_empty() => {
                require_closed(&pending, sequenced.seq)?;
                flush_deferred_inputs(&mut deferred_inputs, &mut out);
                push(
                    &mut out,
                    ModelSurfaceMessage::Assistant { text: text.clone() },
                    sequenced.seq,
                );
            }
            Event::ModelToolCallsRequested { request_id, calls } => {
                require_closed(&pending, sequenced.seq)?;
                if calls.is_empty() {
                    return Err(invalid("model tool-call batch must not be empty"));
                }
                flush_deferred_inputs(&mut deferred_inputs, &mut out);
                for call in calls {
                    register_call(call, sequenced.seq, &mut pending, &mut declared, &completed)?;
                }
                push(
                    &mut out,
                    ModelSurfaceMessage::AssistantToolCalls {
                        request_id: request_id.clone(),
                        calls: calls.clone(),
                    },
                    sequenced.seq,
                );
            }
            Event::ToolResultAdded { call_id, outcome } => {
                if let Some(_requested_at) = pending.remove(call_id) {
                    if !completed.insert(call_id.clone()) {
                        return Err(invalid(format!(
                            "duplicate model tool result for call {call_id}"
                        )));
                    }
                    push(
                        &mut out,
                        ModelSurfaceMessage::ToolResult {
                            call_id: call_id.clone(),
                            outcome: outcome.clone(),
                        },
                        sequenced.seq,
                    );
                } else if completed.contains(call_id) {
                    return Err(invalid(format!(
                        "duplicate model tool result for call {call_id}"
                    )));
                }
            }
            Event::MemoryContextInjected { summary } => {
                require_closed(&pending, sequenced.seq)?;
                flush_deferred_inputs(&mut deferred_inputs, &mut out);
                // 记忆注入是加法式系统消息：进模型表面但绝不参与压缩替换
                out.insert(
                    0,
                    ModelSurfaceEntry {
                        message: ModelSurfaceMessage::SystemSummary {
                            text: summary.clone(),
                        },
                        source_event_seqs: vec![sequenced.seq],
                    },
                );
            }
            Event::CompactionApplied {
                summary,
                compacted_messages,
                source_event_seqs,
            } => {
                require_closed(&pending, sequenced.seq)?;
                flush_deferred_inputs(&mut deferred_inputs, &mut out);
                match compacted_messages {
                    Some(count) => apply_exact_compaction(
                        &mut out,
                        *count,
                        source_event_seqs,
                        summary,
                        sequenced.seq,
                    )?,
                    None => out.insert(
                        0,
                        ModelSurfaceEntry {
                            message: ModelSurfaceMessage::SystemSummary {
                                text: summary.clone(),
                            },
                            source_event_seqs: vec![sequenced.seq],
                        },
                    ),
                }
            }
            _ => {}
        }
    }
    require_closed(&pending, events.last().map_or(0, |event| event.seq + 1))?;
    flush_deferred_inputs(&mut deferred_inputs, &mut out);
    Ok(out)
}

/// TASK-704：把已闭合批次的 steer 输入按入队顺序投递为 User 消息。
fn flush_deferred_inputs(deferred: &mut Vec<(u64, String)>, out: &mut Vec<ModelSurfaceEntry>) {
    for (seq, text) in deferred.drain(..) {
        push(out, ModelSurfaceMessage::User { text }, seq);
    }
}

fn register_call(
    call: &ModelToolCall,
    seq: u64,
    pending: &mut BTreeMap<String, u64>,
    declared: &mut BTreeSet<String>,
    completed: &BTreeSet<String>,
) -> Result<(), ErrorEnvelope> {
    if call.id.trim().is_empty() || call.name.trim().is_empty() {
        return Err(invalid("model tool call id/name must not be empty"));
    }
    if completed.contains(&call.id) || !declared.insert(call.id.clone()) {
        return Err(invalid(format!("duplicate model tool call id {}", call.id)));
    }
    pending.insert(call.id.clone(), seq);
    Ok(())
}

fn apply_exact_compaction(
    out: &mut Vec<ModelSurfaceEntry>,
    count: u64,
    source_event_seqs: &[u64],
    summary: &str,
    event_seq: u64,
) -> Result<(), ErrorEnvelope> {
    let count = usize::try_from(count).map_err(|_| invalid("compaction count overflows usize"))?;
    if count == 0 || count > out.len() {
        return Err(invalid(format!(
            "compaction prefix {count} is outside surface length {}",
            out.len()
        )));
    }
    require_pair_complete(&out[..count])?;
    let expected = collect_sources(&out[..count]);
    if expected != source_event_seqs {
        return Err(invalid(
            "compaction source events do not match replaced prefix",
        ));
    }
    out.drain(..count);
    out.insert(
        0,
        ModelSurfaceEntry {
            message: ModelSurfaceMessage::SystemSummary {
                text: summary.to_string(),
            },
            source_event_seqs: vec![event_seq],
        },
    );
    Ok(())
}

fn require_pair_complete(entries: &[ModelSurfaceEntry]) -> Result<(), ErrorEnvelope> {
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();
    for entry in entries {
        match &entry.message {
            ModelSurfaceMessage::AssistantToolCalls { calls: batch, .. } => {
                calls.extend(batch.iter().map(|call| call.id.clone()));
            }
            ModelSurfaceMessage::ToolResult { call_id, .. } => {
                results.insert(call_id.clone());
            }
            _ => {}
        }
    }
    if calls != results {
        return Err(invalid("compaction prefix splits a tool call/result pair"));
    }
    Ok(())
}

pub(crate) fn collect_sources(entries: &[ModelSurfaceEntry]) -> Vec<u64> {
    let mut seen = BTreeSet::new();
    entries
        .iter()
        .flat_map(|entry| entry.source_event_seqs.iter().copied())
        .filter(|seq| seen.insert(*seq))
        .collect()
}

fn require_closed(pending: &BTreeMap<String, u64>, seq: u64) -> Result<(), ErrorEnvelope> {
    if pending.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "model surface reached event {seq} with unmatched tool calls"
        )))
    }
}

fn push(out: &mut Vec<ModelSurfaceEntry>, message: ModelSurfaceMessage, seq: u64) {
    out.push(ModelSurfaceEntry {
        message,
        source_event_seqs: vec![seq],
    });
}

fn invalid(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ToolOutcome;
    use serde_json::json;

    fn se(seq: u64, event: Event) -> SequencedEvent {
        SequencedEvent { seq, event }
    }

    fn call() -> ModelToolCall {
        ModelToolCall {
            id: "c1".into(),
            name: "lookup".into(),
            arguments: r#"{"q":"rust"}"#.into(),
        }
    }

    #[test]
    fn projects_explicit_model_tools_and_ignores_audit_tools() {
        let events = vec![
            se(0, Event::UserMessage { text: "q".into() }),
            se(
                1,
                Event::ModelToolCallsRequested {
                    request_id: "r1".into(),
                    calls: vec![call()],
                },
            ),
            se(
                2,
                Event::ToolCallRequested {
                    call_id: "c1".into(),
                    tool: "lookup".into(),
                    args: json!({"q":"rust"}),
                },
            ),
            se(
                3,
                Event::ToolResultAdded {
                    call_id: "c1".into(),
                    outcome: ToolOutcome::Success { value: json!("ok") },
                },
            ),
            se(
                4,
                Event::ToolCallRequested {
                    call_id: "hook-1".into(),
                    tool: "hook:audit".into(),
                    args: json!({}),
                },
            ),
            se(
                5,
                Event::ToolResultAdded {
                    call_id: "hook-1".into(),
                    outcome: ToolOutcome::Success { value: json!(null) },
                },
            ),
            se(
                6,
                Event::AssistantMessage {
                    text: "done".into(),
                },
            ),
        ];
        let projected = project_model_surface(&events).unwrap();
        assert_eq!(projected.len(), 4);
        assert!(matches!(
            projected[1].message,
            ModelSurfaceMessage::AssistantToolCalls { .. }
        ));
        assert!(matches!(
            projected[2].message,
            ModelSurfaceMessage::ToolResult { .. }
        ));
    }

    #[test]
    fn exact_compaction_replaces_prefix_and_checks_sources() {
        let events = vec![
            se(0, Event::UserMessage { text: "old".into() }),
            se(
                1,
                Event::AssistantMessage {
                    text: "answer".into(),
                },
            ),
            se(
                2,
                Event::UserMessage {
                    text: "recent".into(),
                },
            ),
            se(
                3,
                Event::CompactionApplied {
                    summary: "summary".into(),
                    compacted_messages: Some(2),
                    source_event_seqs: vec![0, 1],
                },
            ),
        ];
        let projected = project_model_surface(&events).unwrap();
        assert_eq!(projected.len(), 2);
        assert!(
            matches!(&projected[0].message, ModelSurfaceMessage::SystemSummary { text } if text == "summary")
        );
        assert!(
            matches!(&projected[1].message, ModelSurfaceMessage::User { text } if text == "recent")
        );
    }

    #[test]
    fn rejects_missing_result_duplicate_result_and_split_pair() {
        let batch = se(
            0,
            Event::ModelToolCallsRequested {
                request_id: "r".into(),
                calls: vec![call()],
            },
        );
        assert!(project_model_surface(std::slice::from_ref(&batch)).is_err());

        let result = se(
            1,
            Event::ToolResultAdded {
                call_id: "c1".into(),
                outcome: ToolOutcome::Success { value: json!(1) },
            },
        );
        assert!(project_model_surface(&[batch.clone(), result.clone(), result]).is_err());

        let compact = se(
            1,
            Event::CompactionApplied {
                summary: "bad".into(),
                compacted_messages: Some(1),
                source_event_seqs: vec![0],
            },
        );
        assert!(project_model_surface(&[batch, compact]).is_err());
    }

    #[test]
    fn legacy_compaction_is_preserved_without_claiming_exact_replacement() {
        let events = vec![
            se(0, Event::UserMessage { text: "q".into() }),
            se(
                1,
                Event::CompactionApplied {
                    summary: "legacy".into(),
                    compacted_messages: None,
                    source_event_seqs: vec![],
                },
            ),
        ];
        let projected = project_model_surface(&events).unwrap();
        assert_eq!(projected.len(), 2);
        assert!(matches!(
            projected[0].message,
            ModelSurfaceMessage::SystemSummary { .. }
        ));
    }
}
