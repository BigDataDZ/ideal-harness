//! P5 / D10：SQLite 只做 JSONL 事件流的可重建投影。

use crate::{replay, JsonlSession, SessionStore};
use protocol::{Event, SequencedEvent};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::io;
use std::path::{Path, PathBuf};

const PROJECTION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
enum SyncOutcome {
    UpToDate,
    Extended { appended: usize },
    Rebuilt,
}

/// 可由 JSONL 完整重建的 SQLite 会话投影。
pub struct SqliteProjection {
    connection: Connection,
}

impl SqliteProjection {
    /// 打开投影数据库并创建固定 schema。
    pub fn open(path: &Path) -> io::Result<Self> {
        let connection = Connection::open(path).map_err(sqlite_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS session_events (
                     seq INTEGER PRIMARY KEY,
                     event_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS projection_metadata (
                     singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                     schema_version INTEGER NOT NULL,
                     source_watermark INTEGER
                 );
                 INSERT INTO projection_metadata(singleton, schema_version, source_watermark)
                 VALUES (1, 1, NULL)
                 ON CONFLICT(singleton) DO NOTHING;",
            )
            .map_err(sqlite_error)?;
        Ok(Self { connection })
    }

    /// 用真相源内容原子替换当前投影。
    pub fn rebuild(&mut self, events: &[SequencedEvent]) -> io::Result<()> {
        let transaction = self.connection.transaction().map_err(sqlite_error)?;
        replace_projection(&transaction, events)?;
        transaction.commit().map_err(sqlite_error)
    }

    /// 按序号幂等写入一条事件；同序号内容不一致会失败，避免掩盖损坏。
    pub fn append(&mut self, event: &SequencedEvent) -> io::Result<()> {
        let transaction = self.connection.transaction().map_err(sqlite_error)?;
        let watermark = metadata(&transaction)?.1;
        if let Some(existing_json) = transaction
            .query_row(
                "SELECT event_json FROM session_events WHERE seq = ?1",
                params![event.seq],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_error)?
        {
            let expected_json = serde_json::to_string(&event.event)?;
            if existing_json == expected_json {
                transaction.commit().map_err(sqlite_error)?;
                return Ok(());
            }
            return Err(invalid_data(format!(
                "projection conflict at sequence {}",
                event.seq
            )));
        }
        let expected_seq = watermark.map_or(0, |seq| seq.saturating_add(1));
        if event.seq != expected_seq {
            return Err(invalid_data(format!(
                "projection gap: expected sequence {expected_seq}, got {}",
                event.seq
            )));
        }
        insert_event(&transaction, event)?;
        update_metadata(&transaction, Some(event.seq))?;
        transaction.commit().map_err(sqlite_error)
    }

    /// 查询按序号排列的完整事件投影。
    pub fn query_events(&self) -> io::Result<Vec<SequencedEvent>> {
        let mut statement = self
            .connection
            .prepare("SELECT seq, event_json FROM session_events ORDER BY seq")
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.map(|row| {
            let (seq, event_json) = row.map_err(sqlite_error)?;
            let event = serde_json::from_str(&event_json)?;
            Ok(SequencedEvent { seq, event })
        })
        .collect()
    }

    fn synchronize(&mut self, source: &[SequencedEvent]) -> io::Result<SyncOutcome> {
        let (schema_version, watermark) = metadata(&self.connection)?;
        let prefix_len = watermark
            .and_then(|seq| usize::try_from(seq).ok())
            .and_then(|seq| seq.checked_add(1))
            .unwrap_or(0);
        if schema_version != PROJECTION_SCHEMA_VERSION
            || prefix_len > source.len()
            || !self.prefix_matches(source, prefix_len)?
        {
            self.rebuild(source)?;
            return Ok(SyncOutcome::Rebuilt);
        }
        if prefix_len == source.len() {
            return Ok(SyncOutcome::UpToDate);
        }
        let suffix = &source[prefix_len..];
        let transaction = self.connection.transaction().map_err(sqlite_error)?;
        insert_events(&transaction, suffix)?;
        update_metadata(&transaction, source.last().map(|event| event.seq))?;
        transaction.commit().map_err(sqlite_error)?;
        Ok(SyncOutcome::Extended {
            appended: suffix.len(),
        })
    }

    fn prefix_matches(&self, source: &[SequencedEvent], prefix_len: usize) -> io::Result<bool> {
        let projected = self.query_events()?;
        if projected.len() != prefix_len || source.len() < prefix_len {
            return Ok(false);
        }
        Ok(projected == source[..prefix_len])
    }
}

/// 先持久化 JSONL、后更新 SQLite 的 write-behind 编排器。
pub struct ProjectedSession {
    source: JsonlSession,
    projection: SqliteProjection,
}

impl ProjectedSession {
    pub fn create(jsonl_path: PathBuf, sqlite_path: &Path) -> io::Result<Self> {
        let events = replay(&jsonl_path)?;
        let source = JsonlSession::create(jsonl_path)?;
        let mut projection = SqliteProjection::open(sqlite_path)?;
        projection.synchronize(&events)?;
        Ok(Self { source, projection })
    }

    /// JSONL 写入成功后才更新投影；投影失败不会撤销真相源。
    pub fn append(&mut self, event: Event) -> io::Result<SequencedEvent> {
        let sequenced = self.source.append(event)?;
        self.projection.append(&sequenced)?;
        Ok(sequenced)
    }

    pub fn query_events(&self) -> io::Result<Vec<SequencedEvent>> {
        self.projection.query_events()
    }

    pub fn source_path(&self) -> &Path {
        self.source.path()
    }
}

impl SessionStore for ProjectedSession {
    fn append(&mut self, event: Event) -> io::Result<SequencedEvent> {
        ProjectedSession::append(self, event)
    }

    fn len(&self) -> u64 {
        self.source.len()
    }

    fn path(&self) -> &Path {
        self.source_path()
    }

    fn replay_events(&self) -> io::Result<Vec<SequencedEvent>> {
        replay(self.source.path())
    }
}

fn replace_projection(transaction: &Transaction<'_>, events: &[SequencedEvent]) -> io::Result<()> {
    transaction
        .execute("DELETE FROM session_events", [])
        .map_err(sqlite_error)?;
    insert_events(transaction, events)?;
    transaction
        .execute(
            "UPDATE projection_metadata
             SET schema_version = ?1, source_watermark = ?2 WHERE singleton = 1",
            params![
                PROJECTION_SCHEMA_VERSION,
                events.last().map(|event| event.seq)
            ],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn insert_events(transaction: &Transaction<'_>, events: &[SequencedEvent]) -> io::Result<()> {
    let mut statement = transaction
        .prepare("INSERT INTO session_events(seq, event_json) VALUES (?1, ?2)")
        .map_err(sqlite_error)?;
    for event in events {
        statement
            .execute(params![event.seq, serde_json::to_string(&event.event)?])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

fn insert_event(transaction: &Transaction<'_>, event: &SequencedEvent) -> io::Result<()> {
    transaction
        .execute(
            "INSERT INTO session_events(seq, event_json) VALUES (?1, ?2)",
            params![event.seq, serde_json::to_string(&event.event)?],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn metadata(connection: &Connection) -> io::Result<(u32, Option<u64>)> {
    connection
        .query_row(
            "SELECT schema_version, source_watermark
             FROM projection_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sqlite_error)
}

fn update_metadata(transaction: &Transaction<'_>, watermark: Option<u64>) -> io::Result<()> {
    transaction
        .execute(
            "UPDATE projection_metadata SET source_watermark = ?1 WHERE singleton = 1",
            params![watermark],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn sqlite_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ih-session-projection-{}-{name}",
            std::process::id()
        ));
        (root.with_extension("jsonl"), root.with_extension("sqlite"))
    }

    fn cleanup(paths: &(PathBuf, PathBuf)) {
        for path in [&paths.0, &paths.1] {
            std::fs::remove_file(path).ok();
            std::fs::remove_file(path.with_extension("sqlite-shm")).ok();
            std::fs::remove_file(path.with_extension("sqlite-wal")).ok();
        }
    }

    fn source_events(count: u64) -> Vec<SequencedEvent> {
        (0..count)
            .map(|seq| SequencedEvent {
                seq,
                event: Event::TurnStarted { turn_id: seq },
            })
            .collect()
    }

    #[test]
    fn projection_query_matches_jsonl_replay() {
        let paths = paths("consistent");
        cleanup(&paths);
        let mut session = ProjectedSession::create(paths.0.clone(), &paths.1).unwrap();
        session.append(Event::TurnStarted { turn_id: 9 }).unwrap();
        session
            .append(Event::UserMessage {
                text: "hello".into(),
            })
            .unwrap();
        assert_eq!(session.query_events().unwrap(), replay(&paths.0).unwrap());
        cleanup(&paths);
    }

    #[test]
    fn long_log_reopen_is_read_only_and_source_ahead_appends_only_suffix() {
        let paths = paths("incremental");
        cleanup(&paths);
        let events = source_events(1_000);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection.rebuild(&events).unwrap();
        let before: u64 = projection
            .connection
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            projection.synchronize(&events).unwrap(),
            SyncOutcome::UpToDate
        );
        let after: u64 = projection
            .connection
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, before);
        let extended = source_events(1_003);
        assert_eq!(
            projection.synchronize(&extended).unwrap(),
            SyncOutcome::Extended { appended: 3 }
        );
        assert_eq!(projection.query_events().unwrap(), extended);
        cleanup(&paths);
    }

    #[test]
    fn sqlite_ahead_is_atomically_rebuilt_from_source() {
        let paths = paths("ahead");
        cleanup(&paths);
        let source = source_events(2);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection.rebuild(&source_events(3)).unwrap();
        assert_eq!(
            projection.synchronize(&source).unwrap(),
            SyncOutcome::Rebuilt
        );
        assert_eq!(projection.query_events().unwrap(), source);
        cleanup(&paths);
    }

    #[test]
    fn middle_gap_and_conflict_each_trigger_rebuild() {
        let paths = paths("gap-conflict");
        cleanup(&paths);
        let source = source_events(4);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection.rebuild(&source).unwrap();
        projection
            .connection
            .execute("DELETE FROM session_events WHERE seq = 1", [])
            .unwrap();
        assert_eq!(
            projection.synchronize(&source).unwrap(),
            SyncOutcome::Rebuilt
        );
        projection
            .connection
            .execute(
                "UPDATE session_events SET event_json = ?1 WHERE seq = 2",
                params![serde_json::to_string(&Event::TurnStarted { turn_id: 99 }).unwrap()],
            )
            .unwrap();
        assert_eq!(
            projection.synchronize(&source).unwrap(),
            SyncOutcome::Rebuilt
        );
        assert_eq!(projection.query_events().unwrap(), source);
        cleanup(&paths);
    }

    #[test]
    fn schema_version_mismatch_triggers_rebuild() {
        let paths = paths("schema");
        cleanup(&paths);
        let source = source_events(2);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection.rebuild(&source).unwrap();
        projection
            .connection
            .execute(
                "UPDATE projection_metadata SET schema_version = 999 WHERE singleton = 1",
                [],
            )
            .unwrap();
        assert_eq!(
            projection.synchronize(&source).unwrap(),
            SyncOutcome::Rebuilt
        );
        assert_eq!(
            metadata(&projection.connection).unwrap().0,
            PROJECTION_SCHEMA_VERSION
        );
        cleanup(&paths);
    }

    #[test]
    fn projection_rejects_conflicting_sequence_content_and_gaps() {
        let paths = paths("invalid-append");
        cleanup(&paths);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection
            .append(&SequencedEvent {
                seq: 0,
                event: Event::TurnStarted { turn_id: 1 },
            })
            .unwrap();
        let conflict = projection
            .append(&SequencedEvent {
                seq: 0,
                event: Event::TurnStarted { turn_id: 2 },
            })
            .unwrap_err();
        assert_eq!(conflict.kind(), io::ErrorKind::InvalidData);
        let gap = projection
            .append(&SequencedEvent {
                seq: 2,
                event: Event::TurnStarted { turn_id: 2 },
            })
            .unwrap_err();
        assert_eq!(gap.kind(), io::ErrorKind::InvalidData);
        cleanup(&paths);
    }
}
