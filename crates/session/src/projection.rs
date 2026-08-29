//! P5 / D10：SQLite 只做 JSONL 事件流的可重建投影。

use crate::{replay, JsonlSession, SessionStore};
use protocol::{Event, SequencedEvent};
use rusqlite::{params, Connection, Transaction};
use std::io;
use std::path::{Path, PathBuf};

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
                 );",
            )
            .map_err(sqlite_error)?;
        Ok(Self { connection })
    }

    /// 用真相源内容原子替换当前投影。
    pub fn rebuild(&mut self, events: &[SequencedEvent]) -> io::Result<()> {
        let transaction = self.connection.transaction().map_err(sqlite_error)?;
        transaction
            .execute("DELETE FROM session_events", [])
            .map_err(sqlite_error)?;
        insert_events(&transaction, events)?;
        transaction.commit().map_err(sqlite_error)
    }

    /// 按序号幂等写入一条事件；同序号内容不一致会失败，避免掩盖损坏。
    pub fn append(&mut self, event: &SequencedEvent) -> io::Result<()> {
        let event_json = serde_json::to_string(&event.event)?;
        let changed = self
            .connection
            .execute(
                "INSERT INTO session_events(seq, event_json) VALUES (?1, ?2)
                 ON CONFLICT(seq) DO UPDATE SET event_json = excluded.event_json
                 WHERE session_events.event_json = excluded.event_json",
                params![event.seq, event_json],
            )
            .map_err(sqlite_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("projection conflict at sequence {}", event.seq),
            ))
        }
    }

    /// 查询按序号排列的完整事件投影。
    pub fn query_events(&self) -> io::Result<Vec<SequencedEvent>> {
        let mut statement = self
            .connection
            .prepare("SELECT seq, event_json FROM session_events ORDER BY seq")
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                let seq = row.get::<_, u64>(0)?;
                let event_json = row.get::<_, String>(1)?;
                Ok((seq, event_json))
            })
            .map_err(sqlite_error)?;

        rows.map(|row| {
            let (seq, event_json) = row.map_err(sqlite_error)?;
            let event = serde_json::from_str(&event_json)?;
            Ok(SequencedEvent { seq, event })
        })
        .collect()
    }
}

/// 先持久化 JSONL、后更新 SQLite 的 write-behind 编排器。
///
/// 打开时总是由 JSONL 重建投影，因此上次运行若在两次写入间崩溃也会自动修复。
pub struct ProjectedSession {
    source: JsonlSession,
    projection: SqliteProjection,
}

impl ProjectedSession {
    pub fn create(jsonl_path: PathBuf, sqlite_path: &Path) -> io::Result<Self> {
        let events = replay(&jsonl_path)?;
        let source = JsonlSession::create(jsonl_path)?;
        let mut projection = SqliteProjection::open(sqlite_path)?;
        projection.rebuild(&events)?;
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
}

fn insert_events(transaction: &Transaction<'_>, events: &[SequencedEvent]) -> io::Result<()> {
    let mut statement = transaction
        .prepare("INSERT INTO session_events(seq, event_json) VALUES (?1, ?2)")
        .map_err(sqlite_error)?;
    for event in events {
        let event_json = serde_json::to_string(&event.event)?;
        statement
            .execute(params![event.seq, event_json])
            .map_err(sqlite_error)?;
    }
    Ok(())
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
        }
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
    fn reopening_repairs_a_stale_projection_from_jsonl() {
        let paths = paths("repair");
        cleanup(&paths);
        {
            let mut source = JsonlSession::create(paths.0.clone()).unwrap();
            source.append(Event::TurnStarted { turn_id: 3 }).unwrap();
        }
        {
            let mut stale = SqliteProjection::open(&paths.1).unwrap();
            stale.rebuild(&[]).unwrap();
            assert!(stale.query_events().unwrap().is_empty());
        }

        let session = ProjectedSession::create(paths.0.clone(), &paths.1).unwrap();
        assert_eq!(session.query_events().unwrap(), replay(&paths.0).unwrap());
        cleanup(&paths);
    }

    #[test]
    fn projection_rejects_conflicting_sequence_content() {
        let paths = paths("conflict");
        cleanup(&paths);
        let mut projection = SqliteProjection::open(&paths.1).unwrap();
        projection
            .append(&SequencedEvent {
                seq: 0,
                event: Event::TurnStarted { turn_id: 1 },
            })
            .unwrap();

        let error = projection
            .append(&SequencedEvent {
                seq: 0,
                event: Event::TurnStarted { turn_id: 2 },
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        cleanup(&paths);
    }
}
