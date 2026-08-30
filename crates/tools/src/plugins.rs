//! TASK-607：可信插件清单、哈希与能力声明校验、有效目录隔离加载。
//! 插件是声明式内容包：manifest 声明其工具能力与 payload 哈希，
//! 注册与调度两个时点都强制「声明 ≤ 清单、内容 = 哈希」，坏插件被隔离而不遮蔽好插件。

use crate::ToolRegistry;
use protocol::{ErrorCode, ErrorEnvelope, ToolOutcome};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_TOOLS_PER_PLUGIN: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_VERSION_BYTES: usize = 64;

/// 插件声明的一个工具能力：manifest 是唯一权威，注册时的 spec 必须与声明完全一致。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginToolDeclaration {
    name: String,
    description: String,
    parameters_schema: Value,
}

impl PluginToolDeclaration {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters_schema(&self) -> &Value {
        &self.parameters_schema
    }
}

/// 通过校验的插件：canonical 路径、payload 哈希指纹与已解析 payload 内容。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedPlugin {
    name: String,
    version: String,
    canonical_dir: PathBuf,
    payload_path: PathBuf,
    payload_hash: u64,
    payload: Value,
    tools: Vec<PluginToolDeclaration>,
}

impl VerifiedPlugin {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn canonical_dir(&self) -> &Path {
        &self.canonical_dir
    }

    /// payload 内容的 fnv1a 指纹；调度前会重新落盘校验以捕获漂移。
    pub fn fingerprint(&self) -> u64 {
        self.payload_hash
    }

    pub fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn tools(&self) -> &[PluginToolDeclaration] {
        &self.tools
    }
}

/// 单插件被隔离的阶段；隔离不影响其他插件加载（坏插件不遮蔽好插件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFailureStage {
    Manifest,
    Containment,
    Hash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailure {
    pub plugin: String,
    pub stage: PluginFailureStage,
    pub error: ErrorEnvelope,
}

#[derive(Debug, Clone)]
pub struct PluginCatalog {
    plugin_root: PathBuf,
    plugins: BTreeMap<String, VerifiedPlugin>,
    failures: Vec<PluginFailure>,
}

/// 插件作者计算 payload 哈希的工具函数；manifest `hash` 字段的唯一合法格式。
pub fn content_hash(bytes: &[u8]) -> String {
    format!("fnv1a:{:016x}", fnv1a(bytes))
}

impl PluginCatalog {
    /// 扫描 `<workspace>/.harness/plugins/*/manifest.json`。
    /// 信任边界（workspace 可 canonical 化、插件根非 symlink 且未逃逸）被破坏时整体硬失败；
    /// 单个插件的清单/包含性/哈希问题只隔离该插件并记录 failures。
    pub fn discover(workspace_root: &Path) -> Result<Self, ErrorEnvelope> {
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|error| io_error("canonicalize workspace", error))?;
        let plugin_root = workspace_root.join(".harness").join("plugins");
        if !plugin_root.exists() {
            return Ok(Self {
                plugin_root,
                plugins: BTreeMap::new(),
                failures: Vec::new(),
            });
        }
        reject_symlink(&plugin_root, "plugin root")?;
        let canonical_root = plugin_root
            .canonicalize()
            .map_err(|error| io_error("canonicalize plugin root", error))?;
        ensure_contained(
            &workspace_root,
            &canonical_root,
            "plugin root escapes workspace",
        )?;

        let mut entries: Vec<_> = fs::read_dir(&canonical_root)
            .map_err(|error| io_error("read plugin directory", error))?
            .collect::<Result<_, _>>()
            .map_err(|error| io_error("read plugin entry", error))?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut plugins: BTreeMap<String, VerifiedPlugin> = BTreeMap::new();
        let mut failures = Vec::new();
        for entry in entries {
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if let Some(verified) =
                verify_plugin_dir(&canonical_root, &entry.path(), &dir_name, &mut failures)
            {
                if let Some(previous) = plugins.remove(&verified.name) {
                    let error = args_error("duplicate plugin name");
                    failures.push(PluginFailure {
                        plugin: previous.name.clone(),
                        stage: PluginFailureStage::Manifest,
                        error: error.clone(),
                    });
                    failures.push(PluginFailure {
                        plugin: verified.name.clone(),
                        stage: PluginFailureStage::Manifest,
                        error,
                    });
                    continue;
                }
                plugins.insert(verified.name.clone(), verified);
            }
        }
        Ok(Self {
            plugin_root: canonical_root,
            plugins,
            failures,
        })
    }

    pub fn plugins(&self) -> impl Iterator<Item = &VerifiedPlugin> {
        self.plugins.values()
    }

    pub fn get(&self, name: &str) -> Option<&VerifiedPlugin> {
        self.plugins.get(name)
    }

    pub fn failures(&self) -> &[PluginFailure] {
        &self.failures
    }

    /// 校验「插件 <plugin> 有权暴露工具 <tool>」：必须仍通过隔离、声明在案，
    /// 且 payload 当场重新落盘校验（symlink/包含性/哈希）——漂移即拒绝。
    pub fn verify_capability(
        &self,
        plugin: &str,
        tool: &str,
    ) -> Result<&PluginToolDeclaration, ErrorEnvelope> {
        let verified = self.plugins.get(plugin).ok_or_else(|| {
            denied(format!(
                "plugin is not verified (quarantined or unknown): {plugin}"
            ))
        })?;
        let declaration = verified
            .tools
            .iter()
            .find(|declaration| declaration.name == tool)
            .ok_or_else(|| {
                denied(format!(
                    "tool is not declared by plugin manifest: {plugin}:{tool}"
                ))
            })?;
        verify_payload_integrity(&self.plugin_root, verified)?;
        Ok(declaration)
    }

    /// 把一个已验证插件的全部声明工具绑定进 registry：执行结果即 payload 内容
    /// （声明式插件不执行任意代码），并安装本目录为调度门。
    pub fn bind_static_tools(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
        plugin: &str,
    ) -> Result<usize, ErrorEnvelope> {
        registry.set_plugin_gate(Arc::clone(self));
        let verified = self
            .get(plugin)
            .ok_or_else(|| denied(format!("plugin is not verified: {plugin}")))?;
        let payload = verified.payload.clone();
        let mut bound = 0;
        for declaration in verified.tools() {
            let spec = crate::ToolSpec {
                name: declaration.name().to_string(),
                description: declaration.description().to_string(),
                parameters_schema: declaration.parameters_schema().clone(),
                escalation_capable: false,
                timeout_ms: None,
            };
            let payload = payload.clone();
            registry.register_plugin_tool(
                plugin,
                spec,
                Box::new(move |_| ToolOutcome::Success {
                    value: payload.clone(),
                }),
            )?;
            bound += 1;
        }
        Ok(bound)
    }
}

/// 目录级校验：无关条目返回 None 且不记失败；坏插件记录隔离原因后返回 None。
fn verify_plugin_dir(
    canonical_root: &Path,
    raw_dir: &Path,
    dir_name: &str,
    failures: &mut Vec<PluginFailure>,
) -> Option<VerifiedPlugin> {
    let metadata = match fs::symlink_metadata(raw_dir) {
        Ok(metadata) => metadata,
        Err(error) => {
            failures.push(PluginFailure {
                plugin: dir_name.to_string(),
                stage: PluginFailureStage::Containment,
                error: io_error("inspect plugin directory", error),
            });
            return None;
        }
    };
    if metadata.file_type().is_symlink() {
        failures.push(PluginFailure {
            plugin: dir_name.to_string(),
            stage: PluginFailureStage::Containment,
            error: denied("plugin directory symlinks are not trusted"),
        });
        return None;
    }
    if !metadata.is_dir() {
        return None;
    }
    match load_plugin(canonical_root, raw_dir, dir_name) {
        Ok(verified) => Some(verified),
        Err((stage, error)) => {
            failures.push(PluginFailure {
                plugin: dir_name.to_string(),
                stage,
                error,
            });
            None
        }
    }
}

fn load_plugin(
    canonical_root: &Path,
    raw_dir: &Path,
    dir_name: &str,
) -> Result<VerifiedPlugin, (PluginFailureStage, ErrorEnvelope)> {
    let canonical_dir = raw_dir.canonicalize().map_err(|error| {
        (
            PluginFailureStage::Containment,
            io_error("canonicalize plugin directory", error),
        )
    })?;
    ensure_contained(
        canonical_root,
        &canonical_dir,
        "plugin directory escapes trusted root",
    )
    .map_err(|error| (PluginFailureStage::Containment, error))?;

    let manifest_path = canonical_dir.join("manifest.json");
    reject_symlink(&manifest_path, "manifest.json")
        .map_err(|error| (PluginFailureStage::Containment, error))?;
    let canonical_manifest = manifest_path.canonicalize().map_err(|error| {
        (
            PluginFailureStage::Containment,
            io_error("canonicalize manifest.json", error),
        )
    })?;
    ensure_contained(
        canonical_root,
        &canonical_manifest,
        "manifest.json escapes trusted root",
    )
    .map_err(|error| (PluginFailureStage::Containment, error))?;
    let manifest_bytes = fs::read(&canonical_manifest).map_err(|error| {
        (
            PluginFailureStage::Manifest,
            io_error("read plugin manifest", error),
        )
    })?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err((
            PluginFailureStage::Manifest,
            args_error("plugin manifest is too large"),
        ));
    }
    let raw: RawManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        (
            PluginFailureStage::Manifest,
            args_error(format!("plugin manifest is not a valid manifest: {error}")),
        )
    })?;
    let manifest = validate_manifest(raw).map_err(|error| (PluginFailureStage::Manifest, error))?;
    if manifest.name != dir_name {
        return Err((
            PluginFailureStage::Manifest,
            args_error("manifest name must match its plugin directory name"),
        ));
    }

    let raw_payload_path = canonical_dir.join(&manifest.payload);
    reject_symlink(&raw_payload_path, "plugin payload")
        .map_err(|error| (PluginFailureStage::Containment, error))?;
    let payload_path = raw_payload_path.canonicalize().map_err(|error| {
        (
            PluginFailureStage::Containment,
            io_error("canonicalize plugin payload", error),
        )
    })?;
    ensure_contained(
        canonical_root,
        &payload_path,
        "plugin payload escapes trusted root",
    )
    .map_err(|error| (PluginFailureStage::Containment, error))?;
    let payload_bytes = fs::read(&payload_path).map_err(|error| {
        (
            PluginFailureStage::Manifest,
            io_error("read plugin payload", error),
        )
    })?;
    if payload_bytes.len() > MAX_PAYLOAD_BYTES {
        return Err((
            PluginFailureStage::Manifest,
            args_error("plugin payload is too large"),
        ));
    }
    let expected = manifest.hash;
    let actual = fnv1a(&payload_bytes);
    if actual != expected {
        return Err((
            PluginFailureStage::Hash,
            denied("plugin payload hash drifted from manifest"),
        ));
    }
    let payload: Value = serde_json::from_slice(&payload_bytes).map_err(|error| {
        (
            PluginFailureStage::Manifest,
            args_error(format!("plugin payload is not valid JSON: {error}")),
        )
    })?;

    Ok(VerifiedPlugin {
        name: manifest.name,
        version: manifest.version,
        canonical_dir,
        payload_path,
        payload_hash: actual,
        payload,
        tools: manifest.tools,
    })
}

/// 调度前/注册后的当场完整性复核：payload 未被替换、未逃逸、哈希未漂移。
fn verify_payload_integrity(root: &Path, plugin: &VerifiedPlugin) -> Result<(), ErrorEnvelope> {
    reject_symlink(&plugin.payload_path, "plugin payload")?;
    let canonical = plugin
        .payload_path
        .canonicalize()
        .map_err(|error| io_error("canonicalize plugin payload", error))?;
    ensure_contained(root, &canonical, "plugin payload escapes trusted root")?;
    if canonical != plugin.payload_path {
        return Err(denied("plugin payload was replaced after discovery"));
    }
    let bytes = fs::read(&canonical).map_err(|error| io_error("read plugin payload", error))?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(args_error("plugin payload is too large"));
    }
    if fnv1a(&bytes) != plugin.payload_hash {
        return Err(denied("plugin payload hash drifted from manifest"));
    }
    Ok(())
}

struct ParsedManifest {
    name: String,
    version: String,
    payload: String,
    hash: u64,
    tools: Vec<PluginToolDeclaration>,
}

fn validate_manifest(raw: RawManifest) -> Result<ParsedManifest, ErrorEnvelope> {
    if !safe_name(&raw.name) {
        return Err(denied(
            "plugin name contains traversal or unsafe characters",
        ));
    }
    if raw.version.is_empty()
        || raw.version.len() > MAX_VERSION_BYTES
        || !raw
            .version
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(args_error(
            "plugin version must be non-empty, short and alphanumeric",
        ));
    }
    if !safe_name(&raw.payload) {
        return Err(denied(
            "plugin payload reference escapes the plugin directory",
        ));
    }
    let hash = parse_hash(&raw.hash)?;
    if raw.tools.len() > MAX_TOOLS_PER_PLUGIN {
        return Err(args_error("plugin declares too many tools"));
    }
    let mut tools = Vec::new();
    let mut seen = BTreeSet::new();
    for raw_tool in raw.tools {
        if !safe_name(&raw_tool.name) {
            return Err(denied(
                "plugin tool name contains traversal or unsafe characters",
            ));
        }
        if !seen.insert(raw_tool.name.clone()) {
            return Err(args_error("duplicate tool declaration in plugin manifest"));
        }
        if raw_tool.description.is_empty() || raw_tool.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(args_error("plugin tool description is empty or too large"));
        }
        if !raw_tool.parameters_schema.is_object() {
            return Err(args_error(
                "plugin tool parameters_schema must be a JSON object",
            ));
        }
        tools.push(PluginToolDeclaration {
            name: raw_tool.name,
            description: raw_tool.description,
            parameters_schema: raw_tool.parameters_schema,
        });
    }
    Ok(ParsedManifest {
        name: raw.name,
        version: raw.version,
        payload: raw.payload,
        hash,
        tools,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    version: String,
    payload: String,
    hash: String,
    tools: Vec<RawToolDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolDeclaration {
    name: String,
    description: String,
    parameters_schema: Value,
}

fn parse_hash(hash: &str) -> Result<u64, ErrorEnvelope> {
    let hex = hash
        .strip_prefix("fnv1a:")
        .ok_or_else(|| args_error("plugin hash must use fnv1a:<16 hex digits> format"))?;
    if hex.len() != 16 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(args_error("plugin hash must be 16 hex digits"));
    }
    u64::from_str_radix(hex, 16).map_err(|_| args_error("plugin hash is not a valid u64"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), ErrorEnvelope> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_error(format!("inspect {label}"), error))?;
    if metadata.file_type().is_symlink() {
        return Err(denied(format!("{label} symlinks are not trusted")));
    }
    Ok(())
}

fn ensure_contained(root: &Path, path: &Path, message: &str) -> Result<(), ErrorEnvelope> {
    if !path.starts_with(root) {
        return Err(denied(message));
    }
    Ok(())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        && name != "."
        && name != ".."
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

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
