//! P5/TASK-410：从事件流派生并校验 parent/child 子代理 lineage。

use protocol::{Event, SequencedEvent, SubagentOutcome, SubagentReportDelivery};
use std::collections::BTreeMap;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentLineage {
    pub task_id: String,
    pub parent_id: String,
    pub child_id: String,
    pub outcome: SubagentOutcome,
    pub report_delivery: Option<SubagentReportDelivery>,
    pub cancellation_reason: Option<String>,
}

struct OpenLineage {
    task_id: String,
    parent_id: String,
    child_id: String,
    report_delivery: Option<SubagentReportDelivery>,
    cancellation_reason: Option<String>,
}

/// 校验每个 Started 都有且仅有一个匹配的 Stopped，并返回闭合 lineage。
pub fn derive_subagent_lineage(events: &[SequencedEvent]) -> io::Result<Vec<SubagentLineage>> {
    let mut open = BTreeMap::<String, OpenLineage>::new();
    let mut closed = Vec::new();
    for record in events {
        match &record.event {
            Event::SubagentStarted {
                task_id,
                parent_id,
                child_id,
            } => {
                if [task_id, parent_id, child_id]
                    .iter()
                    .any(|value| value.trim().is_empty())
                    || open.contains_key(task_id)
                    || closed
                        .iter()
                        .any(|entry: &SubagentLineage| entry.task_id == *task_id)
                {
                    return Err(invalid(format!(
                        "invalid or duplicate subagent start at sequence {}",
                        record.seq
                    )));
                }
                open.insert(
                    task_id.clone(),
                    OpenLineage {
                        task_id: task_id.clone(),
                        parent_id: parent_id.clone(),
                        child_id: child_id.clone(),
                        report_delivery: None,
                        cancellation_reason: None,
                    },
                );
            }
            Event::SubagentCancellationRequested {
                task_id,
                child_id,
                reason,
            } => {
                let entry = active(&mut open, task_id, child_id, record.seq)?;
                if reason.trim().is_empty()
                    || entry.cancellation_reason.replace(reason.clone()).is_some()
                {
                    return Err(invalid(format!(
                        "invalid duplicate cancellation at sequence {}",
                        record.seq
                    )));
                }
            }
            Event::SubagentReportDelivered {
                task_id,
                child_id,
                delivery,
                ..
            } => {
                let entry = active(&mut open, task_id, child_id, record.seq)?;
                if entry.report_delivery.replace(*delivery).is_some() {
                    return Err(invalid(format!(
                        "duplicate subagent report at sequence {}",
                        record.seq
                    )));
                }
            }
            Event::SubagentStopped {
                task_id,
                child_id,
                outcome,
            } => {
                let entry = open.remove(task_id).ok_or_else(|| {
                    invalid(format!(
                        "subagent stopped without start at sequence {}",
                        record.seq
                    ))
                })?;
                if entry.child_id != *child_id
                    || (*outcome == SubagentOutcome::Cancelled)
                        != entry.cancellation_reason.is_some()
                    || (*outcome == SubagentOutcome::Succeeded) != entry.report_delivery.is_some()
                {
                    return Err(invalid(format!(
                        "inconsistent subagent stop at sequence {}",
                        record.seq
                    )));
                }
                closed.push(SubagentLineage {
                    task_id: entry.task_id,
                    parent_id: entry.parent_id,
                    child_id: entry.child_id,
                    outcome: *outcome,
                    report_delivery: entry.report_delivery,
                    cancellation_reason: entry.cancellation_reason,
                });
            }
            _ => {}
        }
    }
    if !open.is_empty() {
        return Err(invalid("unclosed subagent lifecycle"));
    }
    Ok(closed)
}

fn active<'a>(
    open: &'a mut BTreeMap<String, OpenLineage>,
    task_id: &str,
    child_id: &str,
    seq: u64,
) -> io::Result<&'a mut OpenLineage> {
    let entry = open
        .get_mut(task_id)
        .ok_or_else(|| invalid(format!("subagent event without start at sequence {seq}")))?;
    if entry.child_id != child_id {
        return Err(invalid(format!(
            "subagent child lineage mismatch at sequence {seq}"
        )));
    }
    Ok(entry)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, event: Event) -> SequencedEvent {
        SequencedEvent { seq, event }
    }

    #[test]
    fn derives_closed_success_lineage_and_old_events_are_ignored() {
        let events = vec![
            record(0, Event::UserMessage { text: "old".into() }),
            record(
                1,
                Event::SubagentStarted {
                    task_id: "t1".into(),
                    parent_id: "root".into(),
                    child_id: "c1".into(),
                },
            ),
            record(
                2,
                Event::SubagentReportDelivered {
                    task_id: "t1".into(),
                    child_id: "c1".into(),
                    delivery: SubagentReportDelivery::Quiet,
                    text: "done".into(),
                },
            ),
            record(
                3,
                Event::SubagentStopped {
                    task_id: "t1".into(),
                    child_id: "c1".into(),
                    outcome: SubagentOutcome::Succeeded,
                },
            ),
        ];
        let lineage = derive_subagent_lineage(&events).unwrap();
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].parent_id, "root");
        assert_eq!(lineage[0].child_id, "c1");
        assert_eq!(
            lineage[0].report_delivery,
            Some(SubagentReportDelivery::Quiet)
        );
    }

    #[test]
    fn unclosed_mismatched_and_inconsistent_lifecycles_are_rejected() {
        let started = record(
            0,
            Event::SubagentStarted {
                task_id: "t1".into(),
                parent_id: "root".into(),
                child_id: "c1".into(),
            },
        );
        assert!(derive_subagent_lineage(std::slice::from_ref(&started)).is_err());
        let mismatched = vec![
            started.clone(),
            record(
                1,
                Event::SubagentStopped {
                    task_id: "t1".into(),
                    child_id: "wrong".into(),
                    outcome: SubagentOutcome::Failed,
                },
            ),
        ];
        assert!(derive_subagent_lineage(&mismatched).is_err());
        let success_without_report = vec![
            started,
            record(
                1,
                Event::SubagentStopped {
                    task_id: "t1".into(),
                    child_id: "c1".into(),
                    outcome: SubagentOutcome::Succeeded,
                },
            ),
        ];
        assert!(derive_subagent_lineage(&success_without_report).is_err());
    }
}
