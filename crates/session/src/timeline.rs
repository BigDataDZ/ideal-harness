//! P5/TASK-408：从事件真相源派生 turn 时间线，并以 fork 实现非破坏性 revert。

use crate::{fork, replay_session, JsonlSession};
use protocol::{Event, SequencedEvent};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Completed,
    Aborted,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub turn_id: u64,
    pub start_seq: u64,
    pub end_seq: Option<u64>,
    pub status: TurnStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelinePage {
    pub turns: Vec<TurnSummary>,
    pub next_cursor: Option<usize>,
}

/// 按时间顺序返回一页 turn；cursor 是稳定的 turn 下标而不是事件序号。
pub fn timeline_page(
    events: &[SequencedEvent],
    cursor: Option<usize>,
    limit: usize,
) -> io::Result<TimelinePage> {
    if limit == 0 {
        return Err(invalid("timeline limit must be greater than zero"));
    }
    let turns = derive_turns(events)?;
    let start = cursor.unwrap_or(0);
    if start > turns.len() || (start == turns.len() && !turns.is_empty()) {
        return Err(invalid(format!(
            "timeline cursor {start} is outside {} turns",
            turns.len()
        )));
    }
    let end = start.saturating_add(limit).min(turns.len());
    Ok(TimelinePage {
        turns: turns[start..end].to_vec(),
        next_cursor: (end < turns.len()).then_some(end),
    })
}

/// 从会话文件重放并返回一页派生时间线。
pub fn timeline_from_session(
    path: &Path,
    cursor: Option<usize>,
    limit: usize,
) -> io::Result<TimelinePage> {
    timeline_page(&replay_session(path)?, cursor, limit)
}

/// fork 源会话中目标 turn 之前的事件；不截断或修改源文件。
pub fn revert_before_turn(
    source: &Path,
    target: PathBuf,
    turn_id: u64,
) -> io::Result<JsonlSession> {
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("source session does not exist: {}", source.display()),
        ));
    }
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("target session already exists: {}", target.display()),
        ));
    }
    let events = replay_session(source)?;
    let turns = derive_turns(&events)?;
    let turn = turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)
        .ok_or_else(|| invalid(format!("unknown turn id {turn_id}")))?;
    let boundary = usize::try_from(turn.start_seq)
        .map_err(|_| invalid(format!("turn {turn_id} boundary exceeds platform limits")))?;
    fork(source, target, boundary)
}

fn derive_turns(events: &[SequencedEvent]) -> io::Result<Vec<TurnSummary>> {
    let mut turns = Vec::new();
    let mut seen = HashSet::new();
    let mut active: Option<usize> = None;
    for (expected_seq, record) in events.iter().enumerate() {
        if record.seq != expected_seq as u64 {
            return Err(invalid(format!(
                "event sequence gap: expected {expected_seq}, got {}",
                record.seq
            )));
        }
        match &record.event {
            Event::TurnStarted { turn_id } => {
                if active.is_some() || !seen.insert(*turn_id) {
                    return Err(invalid(format!(
                        "invalid or duplicate turn start {turn_id} at sequence {}",
                        record.seq
                    )));
                }
                turns.push(TurnSummary {
                    turn_id: *turn_id,
                    start_seq: record.seq,
                    end_seq: None,
                    status: TurnStatus::Active,
                });
                active = Some(turns.len() - 1);
            }
            Event::TurnCompleted { turn_id } => {
                close_turn(
                    &mut turns,
                    &mut active,
                    *turn_id,
                    record.seq,
                    TurnStatus::Completed,
                )?;
            }
            Event::TurnAborted { turn_id, .. } => {
                close_turn(
                    &mut turns,
                    &mut active,
                    *turn_id,
                    record.seq,
                    TurnStatus::Aborted,
                )?;
            }
            _ => {}
        }
    }
    Ok(turns)
}

fn close_turn(
    turns: &mut [TurnSummary],
    active: &mut Option<usize>,
    turn_id: u64,
    end_seq: u64,
    status: TurnStatus,
) -> io::Result<()> {
    let index = active
        .take()
        .ok_or_else(|| invalid(format!("turn {turn_id} ended without a start")))?;
    let turn = &mut turns[index];
    if turn.turn_id != turn_id {
        return Err(invalid(format!(
            "turn {turn_id} ended while turn {} was active",
            turn.turn_id
        )));
    }
    turn.end_seq = Some(end_seq);
    turn.status = status;
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay;

    fn records(turns: u64) -> Vec<SequencedEvent> {
        let mut records = Vec::new();
        for turn_id in 0..turns {
            records.push(SequencedEvent {
                seq: records.len() as u64,
                event: Event::TurnStarted { turn_id },
            });
            records.push(SequencedEvent {
                seq: records.len() as u64,
                event: Event::UserMessage {
                    text: format!("question-{turn_id}"),
                },
            });
            records.push(SequencedEvent {
                seq: records.len() as u64,
                event: Event::TurnCompleted { turn_id },
            });
        }
        records
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-timeline-{}-{name}", std::process::id()))
    }

    #[test]
    fn paginated_timeline_has_no_duplicates_or_gaps() {
        let records = records(5);
        let first = timeline_page(&records, None, 2).unwrap();
        let second = timeline_page(&records, first.next_cursor, 2).unwrap();
        let third = timeline_page(&records, second.next_cursor, 2).unwrap();
        let ids: Vec<_> = first
            .turns
            .iter()
            .chain(&second.turns)
            .chain(&third.turns)
            .map(|turn| turn.turn_id)
            .collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
        assert_eq!(third.next_cursor, None);
    }

    #[test]
    fn invalid_cursor_limit_and_turn_structure_fail_closed() {
        let records = records(2);
        assert_eq!(
            timeline_page(&records, Some(2), 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            timeline_page(&records, None, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let malformed = vec![SequencedEvent {
            seq: 0,
            event: Event::TurnCompleted { turn_id: 9 },
        }];
        assert!(timeline_page(&malformed, None, 1).is_err());
    }

    #[test]
    fn revert_forks_before_turn_and_sessions_evolve_independently() {
        let source = tmp("source.jsonl");
        let target = tmp("target.jsonl");
        let _ = (std::fs::remove_file(&source), std::fs::remove_file(&target));
        let mut parent = JsonlSession::create(source.clone()).unwrap();
        for record in records(3) {
            parent.append(record.event).unwrap();
        }
        let source_before = std::fs::read(&source).unwrap();
        let mut child = revert_before_turn(&source, target.clone(), 2).unwrap();
        assert_eq!(child.len(), 6);
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        parent.append(Event::TurnStarted { turn_id: 3 }).unwrap();
        child.append(Event::TurnStarted { turn_id: 20 }).unwrap();
        assert_eq!(
            replay(&source).unwrap().last().unwrap().event,
            Event::TurnStarted { turn_id: 3 }
        );
        assert_eq!(
            replay(&target).unwrap().last().unwrap().event,
            Event::TurnStarted { turn_id: 20 }
        );
        let _ = (std::fs::remove_file(source), std::fs::remove_file(target));
    }

    #[test]
    fn unknown_turn_and_existing_target_are_rejected_without_mutation() {
        let source = tmp("reject-source.jsonl");
        let target = tmp("reject-target.jsonl");
        let _ = (std::fs::remove_file(&source), std::fs::remove_file(&target));
        let mut parent = JsonlSession::create(source.clone()).unwrap();
        for record in records(1) {
            parent.append(record.event).unwrap();
        }
        assert!(revert_before_turn(&source, target.clone(), 99).is_err());
        assert!(!target.exists());
        std::fs::write(&target, b"owned").unwrap();
        let error = revert_before_turn(&source, target.clone(), 0)
            .err()
            .expect("existing target must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"owned");
        let _ = (std::fs::remove_file(source), std::fs::remove_file(target));
    }
}
