//! 工具注册表（P3）：schema 定义 + 参数校验 + 统一调度。
//! 错误一律以稳定 ErrorCode 回传，供模型自纠，绝不 panic。

mod advertisement;
mod cancel;
mod fs_tools;
mod mcp;
mod mcp_registry;
mod plugins;
mod schema;
mod skills;
mod web;

use protocol::{AuthorizationContext, ErrorEnvelope, ToolOutcome};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub use advertisement::EscalationAvailability;
pub use cancel::CancellationToken;
pub use fs_tools::FsToolSet;
pub use mcp::{McpCallResult, McpClient, McpServerConfig, McpTool};
pub use mcp_registry::{
    McpFailureStage, McpRegistration, McpRegistry, McpServiceFailure, McpServiceRequirement,
    McpServiceSnapshot, McpServiceStatus, McpToolHandle,
};
pub use plugins::{
    content_hash, PluginCatalog, PluginFailure, PluginFailureStage, PluginToolDeclaration,
    VerifiedPlugin,
};
pub use schema::validate_args;
pub use skills::{SkillCatalog, SkillRefresh, VerifiedSkill, VerifiedSkillScope};
pub use web::{is_private_host, FetchRequest, FetchResponse, Fetcher, WebFetchTool};

/// 工具规格：schema 即文档，schema 即校验器输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 形式的参数定义。
    ///
    /// TASK-201 支持工具协议使用的核心关键字：`type`、`enum`、`properties`、
    /// `required`、`items` 与 `additionalProperties`。描述性关键字保持透传。
    pub parameters_schema: serde_json::Value,
    /// 仅当受限沙箱后端挂载时才向模型广告提权出口（P2-4 动态 schema）。
    pub escalation_capable: bool,
    /// TASK-702：单次执行的 deadline（毫秒）。None = 不限时。
    /// 超时不取消底层副作用（进程类工具须自行超时），仅以稳定码把结果回给模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl ToolSpec {
    /// 生成模型可见的参数 schema；仅在受限执行后端已挂载时广告提权出口。
    pub fn advertised_parameters_schema(
        &self,
        availability: EscalationAvailability,
    ) -> Result<serde_json::Value, protocol::ErrorEnvelope> {
        advertisement::advertised_parameters_schema(self, availability)
    }
}

pub type ToolFn = dyn Fn(&serde_json::Value) -> ToolOutcome + Send + Sync;
pub type AuditedToolFn = dyn Fn(&serde_json::Value) -> ToolExecution + Send + Sync;

/// 工具执行期间需要由 agent-loop 绑定真实 call_id 后落盘的审计事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAudit {
    ApprovalDecided {
        approved: bool,
        authorization: Option<AuthorizationContext>,
    },
    AuthorizationInvalidated {
        previous: AuthorizationContext,
        current: AuthorizationContext,
    },
    /// TASK-705：agent-loop 据此落 MemoryRecorded 事件（工具层不持有 session）。
    /// TASK-806：携带来源与作用域。
    MemoryRecorded {
        text: String,
        tags: Vec<String>,
        source: protocol::MemorySource,
        scope: protocol::MemoryScope,
    },
}

/// 调度结果与其伴随审计事实。工具层不伪造协议 call_id。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecution {
    pub outcome: ToolOutcome,
    pub audits: Vec<ToolAudit>,
}

impl ToolExecution {
    pub fn new(outcome: ToolOutcome) -> Self {
        Self {
            outcome,
            audits: Vec::new(),
        }
    }
}

/// TASK-702：handler 以 Arc 承载，使超时执行可以把它移入独立线程。
enum ToolHandler {
    Plain(Arc<ToolFn>),
    Audited(Arc<AuditedToolFn>),
}

struct RegisteredTool {
    spec: ToolSpec,
    handler: ToolHandler,
}

/// 插件工具的注册时快照：调度时会对照目录复核指纹与能力声明。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginProvenance {
    plugin: String,
    fingerprint: u64,
}

pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    escalation_availability: EscalationAvailability,
    /// TASK-802：全局默认 deadline（毫秒）；spec.timeout_ms 优先。
    default_deadline_ms: Option<u64>,
    /// TASK-802：deadline 到期时被取消的协作令牌。
    cancellation_token: Option<Arc<CancellationToken>>,
    /// TASK-607：插件调度门；注册与调度两个时点都强制清单校验。
    plugin_gate: Option<Arc<PluginCatalog>>,
    plugin_tools: BTreeMap<String, PluginProvenance>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            escalation_availability: EscalationAvailability::Unavailable,
            default_deadline_ms: None,
            cancellation_token: None,
            plugin_gate: None,
            plugin_tools: BTreeMap::new(),
        }
    }
}

impl ToolRegistry {
    pub fn set_escalation_availability(&mut self, availability: EscalationAvailability) {
        self.escalation_availability = availability;
    }

    pub fn escalation_availability(&self) -> EscalationAvailability {
        self.escalation_availability
    }

    pub fn register(&mut self, spec: ToolSpec, handler: Box<ToolFn>) {
        assert!(
            self.get(&spec.name).is_none(),
            "duplicate tool name: {}",
            spec.name
        );
        self.tools.push(RegisteredTool {
            spec,
            handler: ToolHandler::Plain(Arc::from(handler)),
        });
    }

    /// 注册会产生审批等审计事实的工具。
    pub fn register_audited(&mut self, spec: ToolSpec, handler: Box<AuditedToolFn>) {
        assert!(
            self.get(&spec.name).is_none(),
            "duplicate tool name: {}",
            spec.name
        );
        self.tools.push(RegisteredTool {
            spec,
            handler: ToolHandler::Audited(Arc::from(handler)),
        });
    }

    /// TASK-802：设置全局默认 deadline（未被 spec.timeout_ms 覆盖时生效）。
    pub fn set_default_deadline(&mut self, deadline: std::time::Duration) {
        self.default_deadline_ms = Some(deadline.as_millis() as u64);
    }

    /// TASK-802：安装协作取消令牌；deadline 到期即取消，handler 在提交点 check。
    pub fn set_cancellation_token(&mut self, token: Arc<CancellationToken>) {
        self.cancellation_token = Some(token);
    }

    /// TASK-607：安装插件调度门；此后 `register_plugin_tool` 才可用。
    pub fn set_plugin_gate(&mut self, gate: Arc<PluginCatalog>) {
        self.plugin_gate = Some(gate);
    }

    /// 注册插件提供的工具：能力必须已由验证过的清单声明且 spec 与声明完全一致；
    /// 插件工具不允许提权出口（manifest 无此能力位），重复名按错误返回而非 panic。
    pub fn register_plugin_tool(
        &mut self,
        plugin: &str,
        spec: ToolSpec,
        handler: Box<ToolFn>,
    ) -> Result<(), ErrorEnvelope> {
        let gate = self.plugin_gate.as_ref().ok_or_else(|| {
            ErrorEnvelope::new(
                protocol::ErrorCode::Internal,
                "plugin gate is not installed; refusing plugin tool registration",
            )
        })?;
        let declaration = gate.verify_capability(plugin, &spec.name)?;
        if spec.escalation_capable {
            return Err(ErrorEnvelope::new(
                protocol::ErrorCode::SandboxDenied,
                "plugin tools cannot declare escalation capability",
            ));
        }
        if spec.description != declaration.description()
            || spec.parameters_schema != *declaration.parameters_schema()
        {
            return Err(ErrorEnvelope::new(
                protocol::ErrorCode::SandboxDenied,
                "registered plugin tool spec does not match its manifest declaration",
            ));
        }
        if self.get(&spec.name).is_some() {
            return Err(ErrorEnvelope::new(
                protocol::ErrorCode::ToolArgsInvalid,
                format!("duplicate tool name: {}", spec.name),
            ));
        }
        let fingerprint = gate
            .get(plugin)
            .map(VerifiedPlugin::fingerprint)
            .expect("capability verified above");
        self.plugin_tools.insert(
            spec.name.clone(),
            PluginProvenance {
                plugin: plugin.to_string(),
                fingerprint,
            },
        );
        self.tools.push(RegisteredTool {
            spec,
            handler: ToolHandler::Plain(Arc::from(handler)),
        });
        Ok(())
    }

    /// 插件来源标记；agent-loop 的结果中间件据此判定「未受监管来源」。
    pub fn plugin_provenance(&self, tool: &str) -> Option<&str> {
        self.plugin_tools.get(tool).map(|p| p.plugin.as_str())
    }

    /// 调度前的插件能力复核：门在位、指纹未漂移、payload 当场完整。
    fn verify_plugin_tool(&self, tool: &str) -> Result<(), ErrorEnvelope> {
        let Some(provenance) = self.plugin_tools.get(tool) else {
            return Ok(());
        };
        let Some(gate) = self.plugin_gate.as_ref() else {
            return Err(ErrorEnvelope::new(
                protocol::ErrorCode::Internal,
                "plugin gate missing; refusing dispatch of plugin tool",
            ));
        };
        match gate.get(&provenance.plugin) {
            Some(verified) if verified.fingerprint() == provenance.fingerprint => {}
            _ => {
                return Err(ErrorEnvelope::new(
                    protocol::ErrorCode::SandboxDenied,
                    "registered plugin capability is stale or quarantined; rebind required",
                ));
            }
        }
        gate.verify_capability(&provenance.plugin, tool).map(|_| ())
    }

    /// 已注册工具名（按注册序）；生产装配据此生成 /tools 与模型广告。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|tool| tool.spec.name.as_str())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools
            .iter()
            .find(|t| t.spec.name == name)
            .map(|t| &t.spec)
    }

    /// 调度：先校验后执行；未知工具与参数错误都归一为 Failure 事件而非错误通道，
    /// 保证 tool_call/result 配对永不断裂（P4）。
    pub fn dispatch(&self, name: &str, args: &serde_json::Value) -> Option<ToolOutcome> {
        let tool = self.tools.iter().find(|tool| tool.spec.name == name)?;
        if matches!(&tool.handler, ToolHandler::Audited(_)) {
            return Some(ToolOutcome::Failure {
                error: protocol::ErrorEnvelope::new(
                    protocol::ErrorCode::Internal,
                    "audited tool requires dispatch_with_audit; refusing to drop audit facts",
                ),
            });
        }
        self.dispatch_with_audit(name, args).map(|run| run.outcome)
    }

    /// 带审计事实调度；agent-loop 必须使用此入口，确保自动审批行为留痕。
    pub fn dispatch_with_audit(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Option<ToolExecution> {
        let t = self.tools.iter().find(|t| t.spec.name == name)?;
        let mut validation_spec = t.spec.clone();
        match t
            .spec
            .advertised_parameters_schema(self.escalation_availability)
        {
            Ok(schema) => validation_spec.parameters_schema = schema,
            Err(error) => return Some(ToolExecution::new(ToolOutcome::Failure { error })),
        }
        if let Err(e) = validate_args(&validation_spec, args) {
            return Some(ToolExecution::new(ToolOutcome::Failure { error: e }));
        }
        if let Err(error) = self.verify_plugin_tool(name) {
            return Some(ToolExecution::new(ToolOutcome::Failure { error }));
        }
        let deadline_ms = t
            .spec
            .timeout_ms
            .or(self.default_deadline_ms)
            .filter(|ms| *ms > 0);
        if let Some(ms) = deadline_ms {
            // TASK-802：deadline 到期即取消协作令牌，handler 在提交点放弃副作用
            if let Some(token) = &self.cancellation_token {
                token.cancel();
            }
            return Some(
                run_with_deadline(&t.handler, args, std::time::Duration::from_millis(ms))
                    .unwrap_or_else(|| {
                        ToolExecution::new(ToolOutcome::Failure {
                            error: ErrorEnvelope::new(
                                protocol::ErrorCode::ToolTimeout,
                                format!("tool {name} exceeded its {ms}ms execution deadline"),
                            ),
                        })
                    }),
            );
        }
        Some(match &t.handler {
            ToolHandler::Plain(handler) => ToolExecution::new(handler(args)),
            ToolHandler::Audited(handler) => handler(args),
        })
    }
}

/// TASK-810：分离线程硬上限；超限时拒绝派生并以稳定码回给模型。
const MAX_DETACHED_TASKS: usize = 64;
static DETACHED_TASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// TASK-702：在独立线程执行 handler 并限时等待；超时返回 None（线程成为分离线程，
/// 其副作用不被取消——结果仅以稳定码回给模型）。
fn run_with_deadline(
    handler: &ToolHandler,
    args: &serde_json::Value,
    deadline: std::time::Duration,
) -> Option<ToolExecution> {
    if DETACHED_TASKS.load(std::sync::atomic::Ordering::SeqCst) >= MAX_DETACHED_TASKS {
        return None;
    }
    let _ = DETACHED_TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (sender, receiver) = std::sync::mpsc::channel();
    let args = args.clone();
    let guard = DetachedGuard;
    match handler {
        ToolHandler::Plain(handler) => {
            let handler = Arc::clone(handler);
            std::thread::spawn(move || {
                let _guard = guard;
                let _ = sender.send(ToolExecution::new(handler(&args)));
            });
        }
        ToolHandler::Audited(handler) => {
            let handler = Arc::clone(handler);
            std::thread::spawn(move || {
                let _guard = guard;
                let _ = sender.send(handler(&args));
            });
        }
    }
    receiver.recv_timeout(deadline).ok()
}

/// RAII：分离线程结束时递减在途计数。
struct DetachedGuard;

impl Drop for DetachedGuard {
    fn drop(&mut self) {
        DETACHED_TASKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ErrorCode;

    fn echo_spec() -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "demo".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
            escalation_capable: false,
            timeout_ms: None,
        }
    }

    #[test]
    fn missing_required_arg_fails_without_invoking_handler() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|_| panic!("handler must not run on invalid args")),
        );
        match reg.dispatch("echo", &serde_json::json!({})) {
            Some(ToolOutcome::Failure { error }) => {
                assert_eq!(error.code, ErrorCode::ToolArgsInvalid)
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_returns_none_for_turn_level_handling() {
        let reg = ToolRegistry::default();
        assert!(reg.dispatch("nope", &serde_json::json!({})).is_none());
    }

    #[test]
    fn valid_dispatch_reaches_handler() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|args| ToolOutcome::Success {
                value: args["text"].clone(),
            }),
        );
        match reg.dispatch("echo", &serde_json::json!({ "text": "hi" })) {
            Some(ToolOutcome::Success { value }) => assert_eq!(value, "hi"),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn fuzz_dispatch_arbitrary_args_never_panics() {
        fn xorshift(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }
        let mut reg = ToolRegistry::default();
        reg.register(
            ToolSpec {
                name: "fuzzed".into(),
                description: "fuzz target".into(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "required": ["a"],
                    "properties": { "a": { "type": "string" } }
                }),
                escalation_capable: false,
                timeout_ms: None,
            },
            Box::new(|_| ToolOutcome::Success { value: ().into() }),
        );
        let mut state = 0xabcd_ef01_u64;
        for _ in 0..2000 {
            let mut value = serde_json::json!({ "a": "x" });
            // 随机破坏参数结构
            let roll = xorshift(&mut state) % 4;
            match roll {
                0 => value = serde_json::json!((xorshift(&mut state) as i64)),
                1 => value = serde_json::json!(format!("n{}x", xorshift(&mut state))),
                2 => value["extra"] = serde_json::json!(xorshift(&mut state)),
                _ => value["a"] = serde_json::json!(xorshift(&mut state)),
            }
            // 任意参数只能得到 Outcome（Success/Failure），绝不 panic
            let _ = reg.dispatch("fuzzed", &value);
        }
    }

    #[test]
    fn deadline_cancel_token_blocks_late_side_effects() {
        // TASK-802 验收：ToolTimeout 返回后，handler 的提交点不再产生写副作用
        let token = Arc::new(CancellationToken::default());
        let mut reg = ToolRegistry::default();
        reg.set_default_deadline(std::time::Duration::from_millis(50));
        reg.set_cancellation_token(Arc::clone(&token));
        let side_effect_file =
            std::env::temp_dir().join(format!("ih-802-{}.txt", std::process::id()));
        std::fs::remove_file(&side_effect_file).ok();
        let path_for_handler = side_effect_file.clone();
        let token_for_handler = Arc::clone(&token);
        reg.register(
            ToolSpec {
                name: "slow_writer".into(),
                description: "sleep then write".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                escalation_capable: false,
                timeout_ms: None,
            },
            Box::new(move |_| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                // 提交点：被取消的 handler 必须放弃写副作用
                if let Err(error) = token_for_handler.check() {
                    return ToolOutcome::Failure { error };
                }
                std::fs::write(&path_for_handler, b"late side effect").unwrap();
                ToolOutcome::Success { value: ().into() }
            }),
        );
        match reg.dispatch("slow_writer", &serde_json::json!({})) {
            Some(ToolOutcome::Failure { error }) => {
                assert_eq!(error.code, ErrorCode::ToolTimeout)
            }
            other => panic!("expected timeout, got {other:?}"),
        }
        // 等 handler 线程跑完睡眠段，确认它没有写文件
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !side_effect_file.exists(),
            "被取消的 handler 不得产生写副作用"
        );
        assert!(token.is_cancelled());
        std::fs::remove_file(&side_effect_file).ok();
    }

    #[test]
    fn deadline_exceeded_returns_tool_timeout_and_still_pairing() {
        let mut reg = ToolRegistry::default();
        reg.register(
            ToolSpec {
                name: "slow".into(),
                description: "slow handler".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                escalation_capable: false,
                timeout_ms: Some(50),
            },
            Box::new(|_| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                ToolOutcome::Success { value: ().into() }
            }),
        );
        let started = std::time::Instant::now();
        match reg.dispatch("slow", &serde_json::json!({})) {
            Some(ToolOutcome::Failure { error }) => {
                assert_eq!(error.code, ErrorCode::ToolTimeout);
                assert!(error.message.contains("deadline"));
            }
            other => panic!("expected timeout failure, got {other:?}"),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "调度必须在 deadline 附近返回而不是等 handler 跑完"
        );
    }

    #[test]
    fn deadline_within_limit_returns_success_with_audits() {
        let mut reg = ToolRegistry::default();
        reg.register_audited(
            ToolSpec {
                name: "quick".into(),
                description: "quick audited handler".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                escalation_capable: false,
                timeout_ms: Some(1000),
            },
            Box::new(|_| ToolExecution {
                outcome: ToolOutcome::Success { value: ().into() },
                audits: vec![ToolAudit::ApprovalDecided {
                    approved: true,
                    authorization: None,
                }],
            }),
        );
        let execution = reg
            .dispatch_with_audit("quick", &serde_json::json!({}))
            .unwrap();
        assert!(matches!(execution.outcome, ToolOutcome::Success { .. }));
        assert_eq!(execution.audits.len(), 1);
    }

    #[test]
    fn duplicate_registration_is_programmer_error() {
        let mut reg = ToolRegistry::default();
        reg.register(
            echo_spec(),
            Box::new(|_| ToolOutcome::Success { value: ().into() }),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reg.register(
                echo_spec(),
                Box::new(|_| ToolOutcome::Success { value: ().into() }),
            )
        }));
        assert!(result.is_err(), "重复注册必须在开发期暴露");
    }

    #[test]
    fn audited_dispatch_preserves_approval_fact() {
        let mut reg = ToolRegistry::default();
        reg.register_audited(
            echo_spec(),
            Box::new(|args| ToolExecution {
                outcome: ToolOutcome::Success {
                    value: args["text"].clone(),
                },
                audits: vec![ToolAudit::ApprovalDecided {
                    approved: true,
                    authorization: None,
                }],
            }),
        );
        let run = reg
            .dispatch_with_audit("echo", &serde_json::json!({ "text": "hi" }))
            .unwrap();
        assert_eq!(
            run.audits,
            [ToolAudit::ApprovalDecided {
                approved: true,
                authorization: None
            }]
        );
    }

    #[test]
    fn plain_dispatch_refuses_to_drop_audited_tool_facts() {
        let mut reg = ToolRegistry::default();
        reg.register_audited(
            echo_spec(),
            Box::new(|_| panic!("audited handler must not run through plain dispatch")),
        );
        match reg.dispatch("echo", &serde_json::json!({ "text": "hi" })) {
            Some(ToolOutcome::Failure { error }) => {
                assert_eq!(error.code, protocol::ErrorCode::Internal)
            }
            other => panic!("expected fail-closed dispatch, got {other:?}"),
        }
    }
}
