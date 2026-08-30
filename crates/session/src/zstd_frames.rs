//! P5 / D10：带提交边界与校验的 zstd append-only 会话格式。

use crate::zstd_record::{persist_payload, scan_records, NEW_FORMAT_MAGIC};
use crate::{replay, SessionStore};
use protocol::{Event, SequencedEvent};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const STORAGE_NAME: &str = "ideal-harness-session";
const STORAGE_VERSION: u64 = 1;
const HEADER_LINE: &[u8] = b"{\"storage\":\"ideal-harness-session\",\"version\":1}\n";

/// 会话迁移的目标编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEncoding {
    Jsonl,
    ZstdFrames,
}

/// 以带 checksum 的独立提交帧追加事件的会话日志。
pub struct ZstdSession {
    path: PathBuf,
    next_seq: u64,
    file: File,
}

impl ZstdSession {
    /// 打开或创建当前格式；旧 TASK-403 流可读和迁移，但必须迁移后才能续写。
    pub fn create(path: PathBuf) -> io::Result<Self> {
        let exists = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let next_seq = if !exists || file.metadata()?.len() == 0 {
            persist_payload(&mut file, HEADER_LINE)?;
            0
        } else if starts_with(&path, &NEW_FORMAT_MAGIC)? {
            repair_and_replay(&path)?.len() as u64
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy zstd session is read-only; migrate it before append",
            ));
        };
        Ok(Self {
            path,
            next_seq,
            file,
        })
    }

    pub fn append(&mut self, event: Event) -> io::Result<SequencedEvent> {
        let mut appended = self.append_batch(vec![event])?;
        appended.pop().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "single append produced no event",
            )
        })
    }

    /// 把一批连续事件写入同一个提交帧；序号只在 sync 成功后前移。
    pub fn append_batch(&mut self, events: Vec<Event>) -> io::Result<Vec<SequencedEvent>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let sequenced: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(offset, event)| SequencedEvent {
                seq: self.next_seq + offset as u64,
                event,
            })
            .collect();
        persist_payload(&mut self.file, &encode_events(&sequenced)?)?;
        self.next_seq += sequenced.len() as u64;
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

    fn replay_events(&self) -> io::Result<Vec<SequencedEvent>> {
        replay_zstd(self.path())
    }
}

/// 自动识别传统 JSONL、旧 zstd 流与当前提交帧格式。
pub fn replay_auto(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    if !path.exists() || path.metadata()?.len() == 0 {
        return Ok(Vec::new());
    }
    if starts_with(path, &NEW_FORMAT_MAGIC)? || starts_with(path, &ZSTD_MAGIC)? {
        replay_zstd(path)
    } else {
        replay(path)
    }
}

/// 重放新旧 zstd 格式；未提交的撕裂尾部只由 `ZstdSession::create` 修复。
pub fn replay_zstd(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    if !path.exists() || path.metadata()?.len() == 0 {
        return Ok(Vec::new());
    }
    if starts_with(path, &NEW_FORMAT_MAGIC)? {
        replay_current(path, false)
    } else if starts_with(path, &ZSTD_MAGIC)? {
        replay_legacy(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session is not a zstd frame stream",
        ))
    }
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

fn repair_and_replay(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    replay_current(path, true)
}

fn replay_current(path: &Path, repair_tail: bool) -> io::Result<Vec<SequencedEvent>> {
    let bytes = std::fs::read(path)?;
    let scan = scan_records(&bytes)?;
    if scan.torn_tail {
        if !repair_tail {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "uncommitted zstd session tail",
            ));
        }
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(scan.valid_len as u64)?;
        file.sync_all()?;
    }
    let (header, event_records) = scan
        .records
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing zstd session header"))?;
    validate_header(header)?;
    let mut events = Vec::new();
    for payload in event_records {
        events.extend(parse_jsonl(BufReader::new(Cursor::new(payload)))?);
    }
    validate_sequence(&events)?;
    Ok(events)
}

fn replay_legacy(path: &Path) -> io::Result<Vec<SequencedEvent>> {
    let decoder = zstd::stream::read::Decoder::new(File::open(path)?)?;
    let events = parse_jsonl(BufReader::new(decoder))?;
    validate_sequence(&events)?;
    Ok(events)
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
            file.sync_all()
        }
        SessionEncoding::ZstdFrames => {
            persist_payload(&mut file, HEADER_LINE)?;
            if !events.is_empty() {
                persist_payload(&mut file, &encode_events(events)?)?;
            }
            Ok(())
        }
    }
}

fn encode_events(events: &[SequencedEvent]) -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    for event in events {
        serde_json::to_writer(&mut payload, event)?;
        payload.push(b'\n');
    }
    Ok(payload)
}

fn validate_header(payload: &[u8]) -> io::Result<()> {
    let value: serde_json::Value = serde_json::from_slice(payload)?;
    if value.get("storage").and_then(serde_json::Value::as_str) != Some(STORAGE_NAME)
        || value.get("version").and_then(serde_json::Value::as_u64) != Some(STORAGE_VERSION)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported zstd session header",
        ));
    }
    Ok(())
}

fn validate_sequence(events: &[SequencedEvent]) -> io::Result<()> {
    if let Some((index, event)) = events
        .iter()
        .enumerate()
        .find(|(index, event)| event.seq != *index as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "non-contiguous event sequence at index {index}: {}",
                event.seq
            ),
        ));
    }
    Ok(())
}

fn parse_jsonl(reader: impl BufRead) -> io::Result<Vec<SequencedEvent>> {
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            events.push(serde_json::from_str(&line)?);
        }
    }
    Ok(events)
}

fn starts_with(path: &Path, expected: &[u8; 4]) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == expected),
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
    use crate::zstd_record::encode_record;
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
    fn batch_append_is_durable_and_reopen_continues_sequence() {
        let path = tmp("append", "zst");
        remove(&[&path]);
        {
            let mut session = ZstdSession::create(path.clone()).unwrap();
            let batch = session
                .append_batch(vec![
                    Event::TurnStarted { turn_id: 4 },
                    Event::UserMessage { text: "hi".into() },
                ])
                .unwrap();
            assert_eq!(batch[1].seq, 1);
        }
        let mut reopened = ZstdSession::create(path.clone()).unwrap();
        assert_eq!(
            reopened
                .append(Event::TurnCompleted { turn_id: 4 })
                .unwrap()
                .seq,
            2
        );
        assert_eq!(replay_zstd(&path).unwrap().len(), 3);
        remove(&[&path]);
    }

    #[test]
    fn reopen_repairs_only_an_uncommitted_tail() {
        let path = tmp("torn", "zst");
        remove(&[&path]);
        let mut session = ZstdSession::create(path.clone()).unwrap();
        session.append(Event::TurnStarted { turn_id: 1 }).unwrap();
        drop(session);
        let committed_len = path.metadata().unwrap().len();
        let torn = encode_record(b"partial").unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&torn[..torn.len() / 2])
            .unwrap();
        assert_eq!(
            replay_zstd(&path).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let recovered = ZstdSession::create(path.clone()).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(path.metadata().unwrap().len(), committed_len);
        remove(&[&path]);
    }

    #[test]
    fn committed_checksum_corruption_is_rejected_not_repaired() {
        let path = tmp("checksum", "zst");
        remove(&[&path]);
        let mut session = ZstdSession::create(path.clone()).unwrap();
        session.append(Event::TurnStarted { turn_id: 1 }).unwrap();
        drop(session);
        let mut bytes = std::fs::read(&path).unwrap();
        let first_len = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
        let second_prefix = 12 + first_len + 12;
        let second_len = u64::from_le_bytes(
            bytes[second_prefix + 4..second_prefix + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        bytes[second_prefix + 12 + second_len / 2] ^= 0x40;
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(
            replay_zstd(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(ZstdSession::create(path.clone()).is_err());
        remove(&[&path]);
    }

    #[test]
    fn legacy_task_403_stream_remains_readable_and_migratable() {
        let legacy = tmp("legacy", "zst");
        let migrated = tmp("migrated", "zst");
        remove(&[&legacy, &migrated]);
        let event = SequencedEvent {
            seq: 0,
            event: Event::UserMessage { text: "old".into() },
        };
        let mut jsonl = serde_json::to_vec(&event).unwrap();
        jsonl.push(b'\n');
        std::fs::write(
            &legacy,
            zstd::stream::encode_all(Cursor::new(jsonl), 3).unwrap(),
        )
        .unwrap();

        assert_eq!(replay_zstd(&legacy).unwrap(), vec![event.clone()]);
        assert!(ZstdSession::create(legacy.clone()).is_err());
        migrate_session(&legacy, &migrated, SessionEncoding::ZstdFrames).unwrap();
        assert_eq!(replay_zstd(&migrated).unwrap(), vec![event]);
        assert!(starts_with(&migrated, &NEW_FORMAT_MAGIC).unwrap());
        remove(&[&legacy, &migrated]);
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
    fn wrong_format_and_existing_target_are_rejected() {
        let wrong = tmp("wrong", "zst");
        let target = tmp("target", "jsonl");
        remove(&[&wrong, &target]);
        std::fs::write(&wrong, b"not-zstd").unwrap();
        assert!(replay_zstd(&wrong).is_err());
        std::fs::write(&target, "owned").unwrap();
        assert_eq!(
            migrate_session(&wrong, &target, SessionEncoding::Jsonl)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "owned");
        remove(&[&wrong, &target]);
    }
}
