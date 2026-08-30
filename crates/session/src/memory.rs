//! TASK-705：跨会话记忆的事件溯源投影。
//! 记忆以 MemoryRecorded 事件写入；重放按 memory_id 幂等（后写覆盖同 id）。

use protocol::{Event, SequencedEvent};

/// 一条已生效的记忆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub memory_id: String,
    pub text: String,
    pub tags: Vec<String>,
}

/// 从事件流投影当前生效的记忆集合；同 id 后写覆盖，输出按 memory_id 稳定排序。
pub fn project_memories(events: &[SequencedEvent]) -> Result<Vec<MemoryEntry>, std::io::Error> {
    let mut by_id = std::collections::BTreeMap::new();
    for sequenced in events {
        if let Event::MemoryRecorded {
            memory_id,
            text,
            tags,
        } = &sequenced.event
        {
            by_id.insert(
                memory_id.clone(),
                MemoryEntry {
                    memory_id: memory_id.clone(),
                    text: text.clone(),
                    tags: tags.clone(),
                },
            );
        }
    }
    Ok(by_id.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SequencedEvent;

    fn sequenced(seq: u64, event: Event) -> SequencedEvent {
        SequencedEvent { seq, event }
    }

    #[test]
    fn projection_is_last_write_wins_per_id_and_stable() {
        let events = vec![
            sequenced(
                0,
                Event::MemoryRecorded {
                    memory_id: "mem-b".into(),
                    text: "旧值".into(),
                    tags: vec![],
                },
            ),
            sequenced(
                1,
                Event::MemoryRecorded {
                    memory_id: "mem-a".into(),
                    text: "偏好 Rust".into(),
                    tags: vec!["lang".into()],
                },
            ),
            sequenced(
                2,
                Event::MemoryRecorded {
                    memory_id: "mem-b".into(),
                    text: "新值".into(),
                    tags: vec!["updated".into()],
                },
            ),
        ];
        let memories = project_memories(&events).unwrap();
        assert_eq!(memories.len(), 2, "同 id 后写覆盖");
        assert_eq!(memories[0].memory_id, "mem-a");
        assert_eq!(memories[1].memory_id, "mem-b");
        assert_eq!(memories[1].text, "新值");
    }

    #[test]
    fn empty_stream_projects_empty_memory() {
        assert!(project_memories(&[]).unwrap().is_empty());
    }
}
