//! TASK-701：内置文件工具集——read/write/edit/glob/grep。
//! 边界 = canonical workspace root（词法 + canonical + symlink 拒绝）；
//! 写路径强制 read-before-write；超限结果全文落 spill 文件，
//! 结果只携带预览 + locator（locator 是工作区内相对路径，可用 fs_read 取回全文）。

use crate::{CancellationToken, ToolRegistry};

/// TASK-804：文件内容摘要（与插件 hash 同一 fnv1a 稳定算法）。
fn content_digest(bytes: &[u8]) -> String {
    format!("fnv1a:{:016x}", fnv1a(bytes))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// TASK-804：CAS 前置——目标存在时必须携带与当前内容一致的 expected_hash。
fn verify_expected_hash(path: &Path, expected: Option<&str>) -> Result<(), ErrorEnvelope> {
    if !path.exists() {
        return Ok(());
    }
    let Some(expected) = expected else {
        return Err(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            format!(
                "expected_hash is required when overwriting an existing file: {}",
                path.display()
            ),
        ));
    };
    let bytes = fs::read(path).map_err(|error| path_error(path, error))?;
    let current = content_digest(&bytes);
    if current != expected.trim() {
        return Err(ErrorEnvelope::new(
            ErrorCode::FileRevisionConflict,
            format!(
                "file changed since last read (expected {expected}, current {current}); file left unchanged"
            ),
        ));
    }
    Ok(())
}

/// TASK-804：原子替换——同目录临时文件 + sync + rename；失败清理临时文件，
/// 路径上只会出现旧文件或完整新文件，绝不出现半文件。
fn atomic_write(path: &Path, bytes: &[u8], unique: u64) -> Result<(), ErrorEnvelope> {
    let file_name = path
        .file_name()
        .ok_or_else(|| args_error("path has no file name"))?
        .to_os_string();
    let mut tmp_name = file_name.clone();
    tmp_name.push(format!(".ih-tmp-{}-{unique}", std::process::id()));
    let tmp_path = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp_path).map_err(|error| path_error(&tmp_path, error))?;
        file.write_all(bytes)
            .map_err(|error| path_error(&tmp_path, error))?;
        file.sync_all()
            .map_err(|error| path_error(&tmp_path, error))?;
    }
    if let Err(error) = fs::rename(&tmp_path, path) {
        fs::remove_file(&tmp_path).ok();
        return Err(path_error(path, error));
    }
    Ok(())
}
use protocol::{ErrorCode, ErrorEnvelope, ToolOutcome};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_WRITE_BYTES: usize = 1024 * 1024;
const MAX_SPILL_BYTES: usize = 8 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 10_000;
const MAX_MATCHES: usize = 200;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const PREVIEW_CHARS: usize = 4_000;
const MAX_WALK_DEPTH: usize = 32;

/// 内置文件工具集合。以 `Arc` 注册进 ToolRegistry，多个工具共享
/// read-before-write 跟踪与 spill 目录。
pub struct FsToolSet {
    workspace_root: PathBuf,
    spill_dir: PathBuf,
    spill_counter: AtomicU64,
    /// 已读取过的 canonical 路径；覆盖写与编辑的前置条件。
    read_tracker: Mutex<BTreeSet<PathBuf>>,
    /// TASK-802：协作取消令牌；写/编辑提交点检查，超时后拒绝产生新副作用。
    cancellation_token: Mutex<Option<CancellationToken>>,
}

impl FsToolSet {
    pub fn new(workspace_root: &Path) -> Result<Arc<Self>, ErrorEnvelope> {
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|error| io_error("canonicalize workspace root", error))?;
        let spill_dir = workspace_root.join(".harness").join("spill");
        Ok(Arc::new(Self {
            workspace_root,
            spill_dir,
            spill_counter: AtomicU64::new(0),
            read_tracker: Mutex::new(BTreeSet::new()),
            cancellation_token: Mutex::new(None),
        }))
    }

    /// TASK-802：安装协作取消令牌；deadline 取消后写/编辑在提交点被拒绝。
    pub fn set_cancellation_token(&self, token: CancellationToken) {
        *self
            .cancellation_token
            .lock()
            .expect("cancel token poisoned") = Some(token);
    }

    fn check_not_cancelled(&self, tool: &str) -> Result<(), ErrorEnvelope> {
        let guard = self
            .cancellation_token
            .lock()
            .expect("cancel token poisoned");
        match guard.as_ref() {
            Some(token) if token.is_cancelled() => Err(ErrorEnvelope::new(
                ErrorCode::ToolTimeout,
                format!("fs_write/fs_edit abandoned: {tool} deadline token cancelled"),
            )),
            _ => Ok(()),
        }
    }

    /// 注册全部文件工具；重复名由 ToolRegistry 断言暴露。
    pub fn register(self: &Arc<Self>, registry: &mut ToolRegistry) {
        type FsToolHandler = fn(&Arc<FsToolSet>, &Value) -> ToolOutcome;
        let specs: Vec<(&str, &str, Value, FsToolHandler)> = vec![
            (
                "fs_read",
                "读取工作区内文本文件；path 相对工作区根；返回 {path, content}",
                serde_json::json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": { "type": "string" } }
                }),
                |set, args| set.tool_read(args),
            ),
            (
            "fs_write",
            "创建或覆盖工作区内文本文件；覆盖既有文件前必须先 fs_read 并携带其返回的 hash 作为 expected_hash；返回 {path, bytes}",
                serde_json::json!({
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    }
                }),
                |set, args| set.tool_write(args),
            ),
            (
            "fs_edit",
            "在工作区文件内做字符串替换；old_string 必须先经 fs_read 并携带其返回的 hash 作为 expected_hash；默认要求唯一匹配，replace_all=true 才全量替换；返回 {path, replacements}",
                serde_json::json!({
                    "type": "object",
                    "required": ["path", "old_string", "new_string"],
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    }
                }),
                |set, args| set.tool_edit(args),
            ),
            (
                "fs_glob",
                "按 glob 模式（支持 * ? 与跨目录 **）列出工作区内文件；返回 {matches}",
                serde_json::json!({
                    "type": "object",
                    "required": ["pattern"],
                    "properties": { "pattern": { "type": "string" } }
                }),
                |set, args| set.tool_glob(args),
            ),
            (
                "fs_grep",
                "在工作区文本文件内做子串搜索（大小写敏感），可先用 glob 过滤文件；返回 {matches:[{file,line,text}]}",
                serde_json::json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "glob": { "type": "string" }
                    }
                }),
                |set, args| set.tool_grep(args),
            ),
        ];
        for (name, description, schema, handler) in specs {
            let set = Arc::clone(self);
            registry.register(
                crate::ToolSpec {
                    name: name.to_string(),
                    description: description.to_string(),
                    parameters_schema: schema,
                    escalation_capable: false,
                    timeout_ms: None,
                },
                Box::new(move |args| handler(&set, args)),
            );
        }
    }

    // ---- 工具入口 ----

    fn tool_read(self: &Arc<Self>, args: &Value) -> ToolOutcome {
        run(|| {
            let path = self.resolve_existing(&string_arg(args, "path")?, "path")?;
            let metadata = fs::metadata(&path).map_err(|error| path_error(&path, error))?;
            let bytes = fs::read(&path).map_err(|error| path_error(&path, error))?;
            let text = decode_utf8(&bytes)?;
            // 内容已（至少部分）交付给模型：计入已读，后续写/编辑合法
            self.read_tracker
                .lock()
                .expect("read tracker poisoned")
                .insert(path.clone());
            let digest = content_digest(&bytes);
            if metadata.len() as usize > MAX_READ_BYTES {
                let locator = self.spill(&text)?;
                return Ok(serde_json::json!({
                    "path": path.display().to_string(),
                    "content": preview(&text),
                    "truncated": true,
                    "locator": locator,
                    "hash": digest,
                }));
            }
            Ok(serde_json::json!({
                "path": path.display().to_string(),
                "content": text,
                "hash": digest,
            }))
        })
    }

    fn tool_write(self: &Arc<Self>, args: &Value) -> ToolOutcome {
        run(|| {
            self.check_not_cancelled("fs_write")?;
            let raw = string_arg(args, "path")?;
            let content = string_arg(args, "content")?;
            if content.len() > MAX_WRITE_BYTES {
                return Err(args_error("content exceeds fs_write size limit"));
            }
            let path = self.resolve_writable(&raw)?;
            if path.exists()
                && !self
                    .read_tracker
                    .lock()
                    .expect("read tracker poisoned")
                    .contains(&path)
            {
                return Err(denied(format!(
                    "read-before-write violated: {} must be read with fs_read before overwriting",
                    path.display()
                )));
            }
            // TASK-804：CAS——覆盖既有文件必须携带与当前内容一致的 expected_hash
            verify_expected_hash(&path, args.get("expected_hash").and_then(Value::as_str))?;
            let unique = self.spill_counter.fetch_add(1, Ordering::Relaxed);
            atomic_write(&path, content.as_bytes(), unique)?;
            self.read_tracker
                .lock()
                .expect("read tracker poisoned")
                .insert(path.clone());
            Ok(serde_json::json!({
                "path": path.display().to_string(),
                "bytes": content.len(),
            }))
        })
    }

    fn tool_edit(self: &Arc<Self>, args: &Value) -> ToolOutcome {
        run(|| {
            self.check_not_cancelled("fs_edit")?;
            let raw = string_arg(args, "path")?;
            let old_string = string_arg(args, "old_string")?;
            let new_string = string_arg(args, "new_string")?;
            let replace_all = args.get("replace_all").and_then(Value::as_bool) == Some(true);
            if old_string.is_empty() {
                return Err(args_error("old_string must not be empty"));
            }
            let path = self.resolve_existing(&raw, "path")?;
            if !self
                .read_tracker
                .lock()
                .expect("read tracker poisoned")
                .contains(&path)
            {
                return Err(denied(format!(
                    "read-before-write violated: {} must be read with fs_read before editing",
                    path.display()
                )));
            }
            let bytes = fs::read(&path).map_err(|error| path_error(&path, error))?;
            // TASK-804：CAS——编辑覆盖前校验 expected_hash（fs_read 返回值中的 hash）
            verify_expected_hash(&path, args.get("expected_hash").and_then(Value::as_str))?;
            let text = decode_utf8(&bytes)?;
            let replacements = text.matches(old_string.as_str()).count();
            if replacements == 0 {
                return Err(args_error("old_string not found; file left unchanged"));
            }
            if replacements > 1 && !replace_all {
                return Err(args_error(format!(
                    "old_string matches {replacements} times; pass replace_all=true or use a longer anchor; file left unchanged"
                )));
            }
            let updated = if replace_all {
                text.replace(old_string.as_str(), new_string.as_str())
            } else {
                text.replacen(old_string.as_str(), new_string.as_str(), 1)
            };
            if updated.len() > MAX_WRITE_BYTES {
                return Err(args_error("edited file would exceed fs_write size limit"));
            }
            let unique = self.spill_counter.fetch_add(1, Ordering::Relaxed);
            atomic_write(&path, updated.as_bytes(), unique)?;
            Ok(serde_json::json!({
                "path": path.display().to_string(),
                "replacements": if replace_all { replacements } else { 1 },
            }))
        })
    }

    fn tool_glob(self: &Arc<Self>, args: &Value) -> ToolOutcome {
        run(|| {
            let pattern = string_arg(args, "pattern")?;
            let segments = parse_pattern(&pattern)?;
            let mut matches: Vec<Value> = Vec::new();
            let mut budget = MAX_WALK_ENTRIES;
            self.walk(&self.workspace_root.clone(), 0, &mut budget, &mut |path| {
                let relative = relative_unix(&self.workspace_root, path);
                if glob_match(&segments, &relative) {
                    matches.push(Value::String(relative));
                }
            })?;
            finish_result_set(self, &mut matches)
        })
    }

    fn tool_grep(self: &Arc<Self>, args: &Value) -> ToolOutcome {
        run(|| {
            let query = string_arg(args, "query")?;
            if query.is_empty() {
                return Err(args_error("query must not be empty"));
            }
            let filter = match args.get("glob") {
                Some(Value::String(pattern)) => Some(parse_pattern(pattern)?),
                _ => None,
            };
            let mut matches: Vec<Value> = Vec::new();
            let mut budget = MAX_WALK_ENTRIES;
            self.walk(&self.workspace_root.clone(), 0, &mut budget, &mut |path| {
                let relative = relative_unix(&self.workspace_root, path);
                if let Some(segments) = &filter {
                    if !glob_match(segments, &relative) {
                        return;
                    }
                }
                let Ok(metadata) = fs::metadata(path) else {
                    return;
                };
                if metadata.len() > MAX_GREP_FILE_BYTES {
                    return;
                }
                let Ok(bytes) = fs::read(path) else {
                    return;
                };
                if bytes.iter().take(8192).any(|byte| *byte == 0) {
                    return; // 二进制文件跳过
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    return;
                };
                for (index, line) in text.lines().enumerate() {
                    if line.contains(query.as_str()) {
                        matches.push(serde_json::json!({
                            "file": relative,
                            "line": index + 1,
                            "text": line.chars().take(500).collect::<String>(),
                        }));
                    }
                }
            })?;
            finish_result_set(self, &mut matches)
        })
    }

    // ---- 内部机制 ----

    /// 深度优先遍历工作区；跳过 `.harness` 与符号链接；受深度与条目预算约束。
    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        budget: &mut usize,
        visit: &mut dyn FnMut(&Path),
    ) -> Result<(), ErrorEnvelope> {
        if depth > MAX_WALK_DEPTH || *budget == 0 {
            return Ok(());
        }
        let entries = fs::read_dir(dir).map_err(|error| io_error("walk directory", error))?;
        let mut collected: Vec<_> = entries
            .collect::<Result<_, _>>()
            .map_err(|error| io_error("walk entry", error))?;
        collected.sort_by_key(|entry| entry.file_name());
        for entry in collected {
            if *budget == 0 {
                return Ok(());
            }
            *budget -= 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if path.file_name().is_some_and(|name| name == ".harness") {
                    continue;
                }
                self.walk(&path, depth + 1, budget, visit)?;
            } else {
                visit(&path);
            }
        }
        Ok(())
    }

    fn resolve_existing(&self, raw: &str, label: &str) -> Result<PathBuf, ErrorEnvelope> {
        let candidate = self.join_within_root(raw, label)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|error| path_error(&candidate, error))?;
        ensure_contained(&self.workspace_root, &canonical, raw)?;
        reject_symlink(&candidate, label)?;
        Ok(canonical)
    }

    fn resolve_writable(&self, raw: &str) -> Result<PathBuf, ErrorEnvelope> {
        let candidate = self.join_within_root(raw, "path")?;
        let parent = candidate
            .parent()
            .ok_or_else(|| args_error("path has no parent directory"))?
            .to_path_buf();
        if !parent.exists() {
            return Err(args_error(format!(
                "parent directory does not exist: {}",
                parent.display()
            )));
        }
        let canonical_parent = parent
            .canonicalize()
            .map_err(|error| path_error(&parent, error))?;
        ensure_contained(&self.workspace_root, &canonical_parent, raw)?;
        reject_symlink(&candidate, "path")?;
        Ok(join_canonical(&canonical_parent, candidate.file_name()))
    }

    fn join_within_root(&self, raw: &str, label: &str) -> Result<PathBuf, ErrorEnvelope> {
        if raw.trim().is_empty() {
            return Err(args_error(format!("{label} must not be empty")));
        }
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Ok(candidate.to_path_buf());
        }
        if raw.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(denied(format!(
                "{label} must not traverse outside the workspace"
            )));
        }
        Ok(self.workspace_root.join(candidate))
    }

    /// 全文落 spill（`.harness/spill/<id>.txt`），返回工作区相对 locator。
    fn spill(&self, content: &str) -> Result<String, ErrorEnvelope> {
        if content.len() > MAX_SPILL_BYTES {
            return Err(args_error("result exceeds spill size limit"));
        }
        fs::create_dir_all(&self.spill_dir)
            .map_err(|error| io_error("create spill directory", error))?;
        let name = format!(
            "fs-{}-{}.txt",
            std::process::id(),
            self.spill_counter.fetch_add(1, Ordering::Relaxed)
        );
        let path = self.spill_dir.join(name);
        let mut file = fs::File::create(&path).map_err(|error| path_error(&path, error))?;
        file.write_all(content.as_bytes())
            .map_err(|error| path_error(&path, error))?;
        Ok(relative_unix(&self.workspace_root, &path))
    }
}

// ---- 结果装配 ----

/// 结果集统一出口：超限时全部渲染行落 spill，返回预览 + locator。
fn finish_result_set(set: &FsToolSet, matches: &mut Vec<Value>) -> Result<Value, ErrorEnvelope> {
    if matches.len() > MAX_MATCHES {
        let rendered: Vec<String> = matches.iter().map(Value::to_string).collect();
        let locator = set.spill(&rendered.join("\n"))?;
        matches.truncate(MAX_MATCHES);
        return Ok(serde_json::json!({
            "matches": matches,
            "truncated": true,
            "locator": locator,
        }));
    }
    Ok(serde_json::json!({ "matches": matches, "truncated": false }))
}

fn run<F>(body: F) -> ToolOutcome
where
    F: FnOnce() -> Result<Value, ErrorEnvelope>,
{
    match body() {
        Ok(value) => ToolOutcome::Success { value },
        Err(error) => ToolOutcome::Failure { error },
    }
}

fn string_arg(args: &Value, key: &str) -> Result<String, ErrorEnvelope> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| args_error(format!("missing string argument: {key}")))
}

fn decode_utf8(bytes: &[u8]) -> Result<String, ErrorEnvelope> {
    String::from_utf8(bytes.to_vec()).map_err(|_| args_error("file is not valid UTF-8 text"))
}

fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}

fn parse_pattern(pattern: &str) -> Result<Vec<String>, ErrorEnvelope> {
    let normalized = pattern.replace('\\', "/");
    if normalized.starts_with('/') || normalized.split('/').any(|segment| segment == "..") {
        return Err(denied("pattern must not traverse outside the workspace"));
    }
    Ok(normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect())
}

fn glob_match(segments: &[String], relative: &str) -> bool {
    let path_segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(segments, &path_segments)
}

fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if first == "**" {
        // ** 匹配零段或多段
        for skip in 0..=path.len() {
            if match_segments(rest, &path[skip..]) {
                return true;
            }
        }
        return false;
    }
    let Some((head, tail)) = path.split_first() else {
        return false;
    };
    segment_match(first, head) && match_segments(rest, tail)
}

fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut p = 0;
    let mut t = 0;
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, t));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn relative_unix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn join_canonical(parent: &Path, file_name: Option<&std::ffi::OsStr>) -> PathBuf {
    match file_name {
        Some(name) => parent.join(name),
        None => parent.to_path_buf(),
    }
}

fn ensure_contained(root: &Path, path: &Path, raw: &str) -> Result<(), ErrorEnvelope> {
    if !path.starts_with(root) {
        return Err(denied(format!("path escapes the workspace root: {raw}")));
    }
    Ok(())
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), ErrorEnvelope> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(denied(format!("{label} symlinks are not followed")));
        }
    }
    Ok(())
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn denied(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::SandboxDenied, message)
}

fn io_error(action: impl AsRef<str>, error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to {}: {error}", action.as_ref()),
    )
}

fn path_error(path: &Path, error: std::io::Error) -> ErrorEnvelope {
    if error.kind() == std::io::ErrorKind::NotFound {
        return args_error(format!("path does not exist: {}", path.display()));
    }
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to access {}: {error}", path.display()),
    )
}

#[cfg(test)]
#[path = "fs_tools_tests.rs"]
mod tests;
