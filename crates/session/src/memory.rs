//! TASK-705：跨会话记忆的事件溯源投影。
//! 记忆以 MemoryRecorded 事件写入；重放按 memory_id 幂等（后写覆盖同 id）。

use protocol::{ErrorCode, ErrorEnvelope, Event, MemoryScope, MemorySource, SequencedEvent};

/// TASK-806：单条记忆大小上限（字节）——超限在写入侧 fail-closed。
pub const MAX_SINGLE_MEMORY_BYTES: usize = 32 * 1024;
/// TASK-806：全部记忆总大小上限（字节）。
pub const MAX_TOTAL_MEMORY_BYTES: usize = 256 * 1024;
/// TASK-806：注入摘要的字符预算——超限 fail-closed（宁可拒绝注入也不静默截断事实）。
pub const MAX_INJECTION_CHARS: usize = 16 * 1024;

/// 一条已生效的记忆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub memory_id: String,
    pub text: String,
    pub tags: Vec<String>,
    pub source: MemorySource,
    pub scope: MemoryScope,
}

/// TASK-806：单条写入的大小守卫（写入侧 fail-closed）。
pub fn validate_memory_size(text: &str) -> Result<(), ErrorEnvelope> {
    if text.len() > MAX_SINGLE_MEMORY_BYTES {
        return Err(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            format!(
                "memory text exceeds the single-memory limit of {MAX_SINGLE_MEMORY_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

/// 从事件流投影当前生效的记忆集合；同 id 后写覆盖，撤销移除，输出按 id 稳定排序。
pub fn project_memories(events: &[SequencedEvent]) -> Result<Vec<MemoryEntry>, std::io::Error> {
    let mut by_id = std::collections::BTreeMap::new();
    for sequenced in events {
        match &sequenced.event {
            Event::MemoryRecorded {
                memory_id,
                text,
                tags,
                source,
                scope,
            } => {
                by_id.insert(
                    memory_id.clone(),
                    MemoryEntry {
                        memory_id: memory_id.clone(),
                        text: text.clone(),
                        tags: tags.clone(),
                        source: *source,
                        scope: *scope,
                    },
                );
            }
            Event::MemoryRevoked { memory_id } => {
                by_id.remove(memory_id);
            }
            _ => {}
        }
    }
    Ok(by_id.into_values().collect())
}

/// TASK-806：渲染注入摘要；总量超限 fail-closed。
/// 每条记忆带来源标注，让模型表面上的事实可审计。
pub fn injection_summary(memories: &[MemoryEntry]) -> Result<String, ErrorEnvelope> {
    let total: usize = memories.iter().map(|m| m.text.len()).sum();
    if total > MAX_TOTAL_MEMORY_BYTES {
        return Err(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            format!("total memory size {total} exceeds the {MAX_TOTAL_MEMORY_BYTES}-byte budget"),
        ));
    }
    let mut out = String::from("Known persistent memories:\n");
    for memory in memories {
        let source = match memory.source {
            MemorySource::Model => "model",
            MemorySource::Host => "host",
        };
        out.push_str(&format!(
            "- [{}][{}] {}\n",
            memory.tags.join(","),
            source,
            memory.text
        ));
    }
    if out.chars().count() > MAX_INJECTION_CHARS {
        return Err(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            format!(
                "memory injection summary exceeds the {MAX_INJECTION_CHARS}-character injection budget"
            ),
        ));
    }
    Ok(out)
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
                    source: MemorySource::Model,
                    scope: MemoryScope::LineageOnly,
                },
            ),
            sequenced(
                1,
                Event::MemoryRecorded {
                    memory_id: "mem-a".into(),
                    text: "偏好 Rust".into(),
                    tags: vec!["lang".into()],
                    source: MemorySource::Model,
                    scope: MemoryScope::LineageOnly,
                },
            ),
            sequenced(
                2,
                Event::MemoryRecorded {
                    memory_id: "mem-b".into(),
                    text: "新值".into(),
                    tags: vec!["updated".into()],
                    source: MemorySource::Model,
                    scope: MemoryScope::LineageOnly,
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
    fn revoke_removes_memory_idempotently_and_budgets_fail_closed() {
        use protocol::MemorySource;
        let events = vec![
            sequenced(
                0,
                Event::MemoryRecorded {
                    memory_id: "mem-a".into(),
                    text: "x".repeat(16),
                    tags: vec![],
                    source: MemorySource::Model,
                    scope: MemoryScope::LineageOnly,
                },
            ),
            sequenced(
                1,
                Event::MemoryRevoked {
                    memory_id: "mem-a".into(),
                },
            ),
            // 对不存在 id 的撤销：幂等无效果
            sequenced(
                2,
                Event::MemoryRevoked {
                    memory_id: "mem-a".into(),
                },
            ),
        ];
        assert!(
            project_memories(&events).unwrap().is_empty(),
            "撤销必须移除记忆"
        );
        // 单条超限 fail-closed
        let oversized = "x".repeat(MAX_SINGLE_MEMORY_BYTES + 1);
        assert!(validate_memory_size(&oversized).is_err());
        assert!(validate_memory_size("ok").is_ok());
        // 总量预算超限 fail-closed
        let memories = vec![MemoryEntry {
            memory_id: "m".into(),
            text: "y".repeat(MAX_TOTAL_MEMORY_BYTES + 1),
            tags: vec![],
            source: MemorySource::Model,
            scope: MemoryScope::LineageOnly,
        }];
        assert!(
            injection_summary(&memories).is_err(),
            "总量超限必须拒绝注入"
        );
    }

    #[test]
    fn empty_stream_projects_empty_memory() {
        assert!(project_memories(&[]).unwrap().is_empty());
    }
}
