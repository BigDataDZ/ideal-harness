//! TASK-412：SQLite 与 zstd 会话恢复的 canonical JSONL 场景快照。

use protocol::{Event, SequencedEvent};
use session::{replay, JsonlSession, ProjectedSession};
use std::path::{Path, PathBuf};

fn tmp(name: &str, extension: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("ih-snapshot-session-{}-{name}", std::process::id()))
        .with_extension(extension)
}

fn assert_snapshot(name: &str, events: &[SequencedEvent]) {
    let actual = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
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
    if expected != actual {
        panic!(
            "snapshot {name} mismatch: {}",
            first_difference(&expected, &actual)
        );
    }
}

fn first_difference(expected: &str, actual: &str) -> String {
    for (index, (left, right)) in expected.lines().zip(actual.lines()).enumerate() {
        if left != right {
            return format!("line {}: expected {left:?}, actual {right:?}", index + 1);
        }
    }
    format!(
        "line count differs: expected {}, actual {}",
        expected.lines().count(),
        actual.lines().count()
    )
}

fn cleanup(path: &Path) {
    std::fs::remove_file(path).ok();
    std::fs::remove_file(path.with_extension("sqlite-wal")).ok();
    std::fs::remove_file(path.with_extension("sqlite-shm")).ok();
}

#[test]
fn sqlite_source_ahead_recovery_trace_matches_snapshot() {
    let jsonl = tmp("sqlite", "jsonl");
    let sqlite = tmp("sqlite", "sqlite");
    cleanup(&jsonl);
    cleanup(&sqlite);
    let mut projected = ProjectedSession::create(jsonl.clone(), &sqlite).unwrap();
    projected.append(Event::TurnStarted { turn_id: 5 }).unwrap();
    projected
        .append(Event::UserMessage { text: "db".into() })
        .unwrap();
    drop(projected);
    let mut source = JsonlSession::create(jsonl.clone()).unwrap();
    source
        .append(Event::AssistantMessage {
            text: "repaired".into(),
        })
        .unwrap();
    source.append(Event::TurnCompleted { turn_id: 5 }).unwrap();
    drop(source);
    let projected = ProjectedSession::create(jsonl.clone(), &sqlite).unwrap();
    assert_snapshot("sqlite-recovery", &projected.query_events().unwrap());
    cleanup(&jsonl);
    cleanup(&sqlite);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_reopen_trace_matches_snapshot() {
    use session::{replay_session, ZstdSession};

    let path = tmp("zstd", "zst");
    cleanup(&path);
    let mut compressed = ZstdSession::create(path.clone()).unwrap();
    compressed
        .append(Event::TurnStarted { turn_id: 7 })
        .unwrap();
    compressed
        .append(Event::UserMessage {
            text: "compressed".into(),
        })
        .unwrap();
    drop(compressed);
    let mut resumed = ZstdSession::create(path.clone()).unwrap();
    resumed
        .append(Event::AssistantMessage {
            text: "resumed".into(),
        })
        .unwrap();
    resumed.append(Event::TurnCompleted { turn_id: 7 }).unwrap();
    drop(resumed);
    assert_snapshot("zstd-reopen", &replay_session(&path).unwrap());
    cleanup(&path);
}

#[test]
fn jsonl_replay_used_by_snapshot_is_canonical() {
    let path = tmp("canonical", "jsonl");
    cleanup(&path);
    let mut source = JsonlSession::create(path.clone()).unwrap();
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    drop(source);
    assert_eq!(replay(&path).unwrap()[0].seq, 0);
    cleanup(&path);
}
