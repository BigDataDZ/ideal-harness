//! P5 / D10：每条事件独立 zstd 帧，保留 append-only 与坏数据即报错语义。

use crate::{replay, SessionStore};
use protocol::{Event, SequencedEvent};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const DEFAULT_LEVEL: i32 = 3;

/// 会话迁移的目标编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEncoding {
    Jsonl,
    ZstdFrames,
}

/// 以独立 zstd 帧追加事件的会话日志。
pub struct ZstdSession {
    path: PathBuf,
    next_seq: u64,
    file: File,
}

impl ZstdSession {
    /// 打开或创建 zstd 会话；已有的非 zstd 文件会被拒绝。
    pub fn create(path: PathBuf) -> io::Result<Self> {
        let existing = replay_zstd(&path)?.len() as u64;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            next_seq: existing,
            file,
        })
    }

    pub fn append(&mut self, event: Event) -> io::Result<SequencedEvent> {
        let sequenced = SequencedEvent {
            seq: self.next_seq,
            event,
        };
        let frame = encode_event(&sequenced)?;
        self.file.write_all(&frame)?;
        self.file.flush()?;
        self.next_seq += 1;
        Ok(sequenced)
    }

    pub fn len(&self) -> u64 {
        self.next_seq
    }

    pub fn is_empty(&self) -> bool {
        self.next_seq == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SessionStore for ZstdSession {
    fn append(&mut self, event: Event) -> io::Result<SequencedEvent> {
        ZstdSession::append(self, event)
    }

    fn len(&self) -> u64 {
        ZstdSession::len(self)
    }

    fn path(&self) -> &Path {
        ZstdSession::path(self)
    }
}

/// 自动识别旧 JSONL 与新 zstd 帧格式并重放。
pub fn replay_auto(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    if !path.exists() || path.metadata()?.len() == 0 {
        return Ok(Vec::new());
    }
    if has_zstd_magic(path)? {
        replay_zstd(path)
    } else {
        replay(path)
    }
}

/// 重放由一个或多个 zstd 帧组成的事件流。
pub fn replay_zstd(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    if !path.exists() || path.metadata()?.len() == 0 {
        return Ok(Vec::new());
    }
    if !has_zstd_magic(path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session is not a zstd frame stream",
        ));
    }

    let decoder = zstd::stream::read::Decoder::new(File::open(path)?)?;
    parse_jsonl(BufReader::new(decoder))
}

/// 将旧/新任一格式迁移到指定编码。源文件始终不变，目标必须不存在。
pub fn migrate_session(source: &Path, target: &Path, encoding: SessionEncoding) -> io::Result<()> {
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "migration target already exists",
        ));
    }
    let events = replay_auto(source)?;
    let temporary = migration_temp_path(target);
    if temporary.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "migration temporary path already exists",
        ));
    }

    let result = write_migration(&temporary, &events, encoding)
        .and_then(|()| std::fs::rename(&temporary, target));
    if result.is_err() {
        std::fs::remove_file(&temporary).ok();
    }
    result
}

fn write_migration(
    path: &Path,
    events: &[SequencedEvent],
    encoding: SessionEncoding,
) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    match encoding {
        SessionEncoding::Jsonl => {
            for event in events {
                writeln!(file, "{}", serde_json::to_string(event)?)?;
            }
        }
        SessionEncoding::ZstdFrames => {
            for event in events {
                file.write_all(&encode_event(event)?)?;
            }
        }
    }
    file.sync_all()
}

fn encode_event(event: &SequencedEvent) -> io::Result<Vec<u8>> {
    let mut jsonl = serde_json::to_vec(event)?;
    jsonl.push(b'\n');
    zstd::stream::encode_all(Cursor::new(jsonl), DEFAULT_LEVEL)
}

fn parse_jsonl(reader: impl BufRead) -> io::Result<Vec<SequencedEvent>> {
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(&line)?);
    }
    Ok(events)
}

fn has_zstd_magic(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == ZSTD_MAGIC),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn migration_temp_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(format!(".migrate-{}.tmp", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsonlSession;

    fn tmp(name: &str, extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ih-session-zstd-{}-{name}.{extension}",
            std::process::id()
        ))
    }

    fn remove(paths: &[&Path]) {
        for path in paths {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn each_append_is_a_replayable_frame_and_reopen_continues_sequence() {
        let path = tmp("append", "zst");
        remove(&[&path]);
        {
            let mut session = ZstdSession::create(path.clone()).unwrap();
            session.append(Event::TurnStarted { turn_id: 4 }).unwrap();
            session
                .append(Event::UserMessage { text: "hi".into() })
                .unwrap();
        }
        let mut reopened = ZstdSession::create(path.clone()).unwrap();
        let event = reopened
            .append(Event::TurnCompleted { turn_id: 4 })
            .unwrap();
        assert_eq!(event.seq, 2);
        assert_eq!(replay_zstd(&path).unwrap().len(), 3);
        assert_eq!(crate::replay_session(&path).unwrap().len(), 3);
        remove(&[&path]);
    }

    #[test]
    fn old_jsonl_and_new_zstd_migrate_both_directions() {
        let old = tmp("old", "jsonl");
        let compressed = tmp("new", "zst");
        let restored = tmp("restored", "jsonl");
        remove(&[&old, &compressed, &restored]);
        let mut session = JsonlSession::create(old.clone()).unwrap();
        session.append(Event::TurnStarted { turn_id: 8 }).unwrap();
        session
            .append(Event::AssistantMessage {
                text: "done".into(),
            })
            .unwrap();
        drop(session);

        migrate_session(&old, &compressed, SessionEncoding::ZstdFrames).unwrap();
        assert_eq!(
            replay_auto(&old).unwrap(),
            replay_auto(&compressed).unwrap()
        );
        migrate_session(&compressed, &restored, SessionEncoding::Jsonl).unwrap();
        assert_eq!(replay_auto(&old).unwrap(), replay_auto(&restored).unwrap());
        remove(&[&old, &compressed, &restored]);
    }

    #[test]
    fn corrupt_or_wrong_format_is_rejected_and_target_is_not_overwritten() {
        let corrupt = tmp("corrupt", "zst");
        let target = tmp("target", "jsonl");
        remove(&[&corrupt, &target]);
        std::fs::write(&corrupt, [0x28, 0xb5, 0x2f, 0xfd, 0xff]).unwrap();
        assert!(replay_zstd(&corrupt).is_err());

        std::fs::write(&target, "owned").unwrap();
        let error = migrate_session(&corrupt, &target, SessionEncoding::Jsonl).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "owned");
        remove(&[&corrupt, &target]);
    }
}
