//! TASK-412：CLI 会话恢复与 fork 的 canonical JSONL 场景快照。

use super::*;
use crate::session_commands::cmd_fork;
use protocol::SequencedEvent;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ih-snapshot-cli-{}-{name}.jsonl",
        std::process::id()
    ))
}

fn assert_snapshot(name: &str, events: &[SequencedEvent]) {
    let actual = canonical_jsonl(events);
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/event-traces")
        .join(format!("{name}.jsonl"));
    let expected = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|error| {
            panic!(
                "missing snapshot {}: {error}\nactual:\n{actual}",
                fixture.display()
            )
        })
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        actual,
        "snapshot {name} differs at {}",
        first_difference(&expected, &actual)
    );
}

fn canonical_jsonl(events: &[SequencedEvent]) -> String {
    events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn first_difference(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<_> = expected.lines().collect();
    let actual_lines: Vec<_> = actual.lines().collect();
    let limit = expected_lines.len().max(actual_lines.len());
    for index in 0..limit {
        if expected_lines.get(index) != actual_lines.get(index) {
            return format!(
                "line {}: expected {:?}, actual {:?}",
                index + 1,
                expected_lines.get(index),
                actual_lines.get(index)
            );
        }
    }
    "byte-level newline difference".into()
}

#[test]
fn resume_recovery_trace_matches_snapshot() {
    let path = tmp("resume");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    session.append(Event::TurnStarted { turn_id: 0 }).unwrap();
    session
        .append(Event::UserMessage {
            text: "hello".into(),
        })
        .unwrap();
    drop(session);
    let mut resumed = JsonlSession::create(path.clone()).unwrap();
    assert!(recover_dangling_turn(&mut resumed).unwrap());
    assert_snapshot("resume-recovery", &replay_session(&path).unwrap());
    std::fs::remove_file(path).ok();
}

#[test]
fn cli_fork_trace_matches_snapshot() {
    let source = tmp("fork-source");
    let target = tmp("fork-target");
    let _ = (std::fs::remove_file(&source), std::fs::remove_file(&target));
    let mut parent = JsonlSession::create(source.clone()).unwrap();
    parent.append(Event::TurnStarted { turn_id: 0 }).unwrap();
    parent
        .append(Event::UserMessage {
            text: "seed".into(),
        })
        .unwrap();
    parent
        .append(Event::AssistantMessage {
            text: "parent".into(),
        })
        .unwrap();
    drop(parent);
    cmd_fork(&[
        "--session".into(),
        source.to_string_lossy().into_owned(),
        "--target".into(),
        target.to_string_lossy().into_owned(),
        "--boundary".into(),
        "2".into(),
    ])
    .unwrap();
    let mut child = JsonlSession::create(target.clone()).unwrap();
    child
        .append(Event::AssistantMessage {
            text: "branch".into(),
        })
        .unwrap();
    child.append(Event::TurnCompleted { turn_id: 0 }).unwrap();
    assert_snapshot("cli-fork", &replay_session(&target).unwrap());
    let _ = (std::fs::remove_file(source), std::fs::remove_file(target));
}

#[test]
fn difference_message_identifies_first_changed_line() {
    assert_eq!(
        first_difference("a\nb\n", "a\nc\n"),
        "line 2: expected Some(\"b\"), actual Some(\"c\")"
    );
}
