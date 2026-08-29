//! P3/TASK-304：超长工具结果外置存储与安全 locator 取回。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillLocator(String);

impl SpillLocator {
    pub fn parse(value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();
        let valid = value.len() == 26
            && value.starts_with("tool-result-")
            && value.ends_with(".txt")
            && value[12..22]
                .chars()
                .all(|character| character.is_ascii_hexdigit());
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid spill locator",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredToolResult {
    Inline(String),
    Spilled {
        preview: String,
        locator: SpillLocator,
        original_bytes: usize,
    },
}

impl StoredToolResult {
    /// 适合放入现有 ToolOutcome::Success value 的事件投影。
    pub fn event_value(&self) -> serde_json::Value {
        match self {
            Self::Inline(value) => serde_json::Value::String(value.clone()),
            Self::Spilled {
                preview,
                locator,
                original_bytes,
            } => serde_json::json!({
                "spilled": true,
                "preview": preview,
                "locator": locator.as_str(),
                "original_bytes": original_bytes,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpillStore {
    root: PathBuf,
    threshold_bytes: usize,
    preview_chars: usize,
}

impl SpillStore {
    pub fn create(root: PathBuf, threshold_bytes: usize, preview_chars: usize) -> io::Result<Self> {
        if threshold_bytes == 0 || preview_chars == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "spill threshold and preview length must be greater than zero",
            ));
        }
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            threshold_bytes,
            preview_chars,
        })
    }

    pub fn store(&self, call_id: &str, full_result: &str) -> io::Result<StoredToolResult> {
        if full_result.len() <= self.threshold_bytes {
            return Ok(StoredToolResult::Inline(full_result.to_string()));
        }
        let hash = fnv1a(call_id.as_bytes().iter().chain(full_result.as_bytes())) & 0xffffffffff;
        let locator = SpillLocator::parse(format!("tool-result-{hash:010x}.txt"))?;
        let final_path = self.root.join(locator.as_str());
        if final_path.exists() {
            let existing = fs::read_to_string(&final_path)?;
            if existing != full_result {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "spill hash collision detected",
                ));
            }
        } else {
            let temp_path =
                self.root
                    .join(format!(".{}.tmp-{}", locator.as_str(), std::process::id()));
            let mut temporary = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            if let Err(error) = temporary
                .write_all(full_result.as_bytes())
                .and_then(|_| temporary.sync_all())
                .and_then(|_| fs::rename(&temp_path, &final_path))
            {
                let _ = fs::remove_file(temp_path);
                return Err(error);
            }
        }
        Ok(StoredToolResult::Spilled {
            preview: full_result.chars().take(self.preview_chars).collect(),
            locator,
            original_bytes: full_result.len(),
        })
    }

    pub fn retrieve(&self, locator: &SpillLocator) -> io::Result<String> {
        fs::read_to_string(self.root.join(locator.as_str()))
    }
}

fn fnv1a<'a>(bytes: impl Iterator<Item = &'a u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-spill-{}-{name}", std::process::id()))
    }

    #[test]
    fn long_result_spills_and_locator_roundtrips_full_content() {
        let root = tmp("roundtrip");
        let _ = fs::remove_dir_all(&root);
        let store = SpillStore::create(root.clone(), 8, 4).unwrap();
        let full = "你好-abcdefghij";
        let stored = store.store("call-1", full).unwrap();
        match &stored {
            StoredToolResult::Spilled {
                preview,
                locator,
                original_bytes,
            } => {
                assert_eq!(preview, "你好-a");
                assert_eq!(*original_bytes, full.len());
                assert_eq!(store.retrieve(locator).unwrap(), full);
                let event = stored.event_value();
                assert_eq!(event["spilled"], true);
                assert_eq!(event["locator"], locator.as_str());
                assert!(!event.to_string().contains("abcdefghij"));
            }
            other => panic!("expected spilled result, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn short_result_stays_inline_without_creating_payload_file() {
        let root = tmp("inline");
        let _ = fs::remove_dir_all(&root);
        let store = SpillStore::create(root.clone(), 8, 4).unwrap();
        let stored = store.store("call-2", "short").unwrap();
        assert_eq!(stored, StoredToolResult::Inline("short".into()));
        assert_eq!(stored.event_value(), "short");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn malformed_locator_and_invalid_configuration_are_rejected() {
        for invalid in ["../secret", "tool-result-xyz.txt", "C:\\secret"] {
            assert!(SpillLocator::parse(invalid).is_err());
        }
        assert!(SpillStore::create(tmp("bad"), 0, 1).is_err());
        assert!(SpillStore::create(tmp("bad2"), 1, 0).is_err());
    }
}
