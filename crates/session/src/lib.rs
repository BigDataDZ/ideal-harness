//! P5：事件溯源。JSONL append-only 是唯一真相源；
//! 崩溃恢复、fork、time-travel 全部由重放派生。

mod lineage;
mod model_surface;
mod projection;
mod spill;
mod timeline;
#[cfg(feature = "zstd")]
mod zstd_frames;
#[cfg(feature = "zstd")]
mod zstd_record;

pub use lineage::{derive_subagent_lineage, SubagentLineage};
pub use model_surface::project_model_surface;
pub use projection::{ProjectedSession, SqliteProjection};
pub use spill::{SpillLocator, SpillStore, StoredToolResult};
pub use timeline::{
    revert_before_turn, timeline_from_session, timeline_page, TimelinePage, TurnStatus, TurnSummary,
};
#[cfg(feature = "zstd")]
pub use zstd_frames::{migrate_session, replay_auto, replay_zstd, SessionEncoding, ZstdSession};

use protocol::{Event, SequencedEvent};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// 会话真相源的最小对象安全接口（TASK-405）。
///
/// AgentLoop 只依赖追加、序号和物理定位，不感知 JSONL/zstd/投影实现。
pub trait SessionStore {
    fn append(&mut self, event: Event) -> std::io::Result<SequencedEvent>;
    fn len(&self) -> u64;
    fn path(&self) -> &Path;
    fn replay_events(&self) -> std::io::Result<Vec<SequencedEvent>>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 追加式会话日志。
pub struct JsonlSession {
    path: PathBuf,
    next_seq: u64,
    file: File,
}

impl JsonlSession {
    /// 打开（不存在则创建）。恢复语义 = 重放到末尾后继续追加。
    pub fn create(path: PathBuf) -> std::io::Result<Self> {
        let existing = replay(&path)?.len() as u64;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            next_seq: existing,
            file,
        })
    }

    pub fn append(&mut self, event: Event) -> std::io::Result<SequencedEvent> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let se = SequencedEvent { seq, event };
        writeln!(self.file, "{}", serde_json::to_string(&se)?).and_then(|_| self.file.flush())?;
        Ok(se)
    }

    /// 已持久化的事件数。
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

impl SessionStore for JsonlSession {
    fn append(&mut self, event: Event) -> std::io::Result<SequencedEvent> {
        JsonlSession::append(self, event)
    }

    fn len(&self) -> u64 {
        JsonlSession::len(self)
    }

    fn path(&self) -> &Path {
        JsonlSession::path(self)
    }

    fn replay_events(&self) -> std::io::Result<Vec<SequencedEvent>> {
        replay(self.path())
    }
}

/// 重放：从磁盘读回全部有序事件。坏行立即报错（不静默跳过——审计优先）。
pub fn replay(path: &Path) -> std::io::Result<Vec<SequencedEvent>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let reader = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line)?);
    }
    Ok(out)
}

/// 统一重放入口：启用 zstd feature 时自动识别压缩格式，否则读取传统 JSONL。
#[cfg(feature = "zstd")]
pub fn replay_session(path: &Path) -> std::io::Result<Vec<SequencedEvent>> {
    replay_auto(path)
}

/// 统一重放入口的无压缩构建版本。
#[cfg(not(feature = "zstd"))]
pub fn replay_session(path: &Path) -> std::io::Result<Vec<SequencedEvent>> {
    replay(path)
}

/// fork（P5）：把源会话前 boundary 个事件复制为种子。
pub fn fork(source: &Path, target: PathBuf, boundary: usize) -> std::io::Result<JsonlSession> {
    let events = replay_session(source)?;
    let mut child = JsonlSession::create(target)?;
    for se in events.into_iter().take(boundary) {
        child.append(se.event)?;
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Event;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-session-{}-{name}", std::process::id()))
    }

    #[test]
    fn append_then_replay_roundtrips_with_sequence() {
        let path = tmp("roundtrip.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut s = JsonlSession::create(path.clone()).unwrap();
        s.append(Event::TurnStarted { turn_id: 1 }).unwrap();
        s.append(Event::UserMessage { text: "hi".into() }).unwrap();

        let events = replay_session(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
        assert_eq!(events[1].event, Event::UserMessage { text: "hi".into() });
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopen_continues_sequence_after_crash() {
        let path = tmp("recover.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = JsonlSession::create(path.clone()).unwrap();
            s.append(Event::TurnStarted { turn_id: 7 }).unwrap();
        } // 模拟崩溃 drop
        let mut s2 = JsonlSession::create(path.clone()).unwrap();
        assert_eq!(s2.len(), 1);
        s2.append(Event::TurnCompleted { turn_id: 7 }).unwrap();
        assert_eq!(replay(&path).unwrap()[1].seq, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fork_copies_prefix_as_seed() {
        let src = tmp("src.jsonl");
        let dst = tmp("dst.jsonl");
        let _ = (std::fs::remove_file(&src), std::fs::remove_file(&dst));
        let mut s = JsonlSession::create(src.clone()).unwrap();
        s.append(Event::TurnStarted { turn_id: 1 }).unwrap();
        s.append(Event::UserMessage { text: "a".into() }).unwrap();
        s.append(Event::UserMessage { text: "b".into() }).unwrap();

        let mut child = fork(&src, dst.clone(), 2).unwrap();
        assert_eq!(child.len(), 2);
        // 子会话独立追加，不影响源
        child.append(Event::TurnCompleted { turn_id: 1 }).unwrap();
        assert_eq!(replay(&src).unwrap().len(), 3);
        assert_eq!(replay(&dst).unwrap().len(), 3);
        let _ = (std::fs::remove_file(&src), std::fs::remove_file(&dst));
    }

    #[test]
    fn corrupt_line_is_an_error_not_silence() {
        let path = tmp("corrupt.jsonl");
        std::fs::write(
            &path,
            "{\"seq\":0,\"event\":{\"type\":\"turn_started\",\"turn_id\":1}}\nGARBAGE\n",
        )
        .unwrap();
        assert!(replay(&path).is_err());
        std::fs::remove_file(&path).ok();
    }
}
