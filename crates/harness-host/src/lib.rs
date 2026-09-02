//! D25/TASK-902: shared production Host assembly for CLI and desktop entry points.

mod desktop_bridge;
mod security;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_loop::{
    AgentLoop, LoopGuard, ToolResultContext, ToolResultDecision, ToolResultMiddleware,
};
use approval::Approver;
pub use model_provider::ProviderProbeFailure;
use model_provider::{ChatMessage, OpenAiCompatClient};
pub use protocol::{ErrorCode, ErrorEnvelope};
use protocol::{Event, ModelCallSpec, ToolOutcome};
use sandbox_exec::PlatformRestrictedBackend;
use session::{replay_session, JsonlSession, SessionStore};
use tools::{
    CancellationToken, EscalationAvailability, ToolAudit, ToolExecution, ToolRegistry, ToolSpec,
};

pub use desktop_bridge::{
    CommandContext, CommandReceipt, DesktopBridge, SessionOperation, SessionReceipt,
};
pub use security::register_exec_tool;
use security::ProviderProxy;

pub const DEFAULT_RESULT_BYTES: usize = 256 * 1024;
pub const DEFAULT_LOOP_REMIND_AFTER: u32 = 3;
pub const DEFAULT_LOOP_REJECT_AFTER: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostConfig {
    pub base_url: String,
    pub model: String,
    pub fetch_allow: Vec<String>,
    pub workspace: PathBuf,
    pub plugin_root: Option<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            fetch_allow: Vec::new(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            plugin_root: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedHostConfig {
    pub base_url: String,
    pub model: String,
    pub fetch_allow: Vec<String>,
    pub workspace: PathBuf,
    pub plugin_root: Option<PathBuf>,
}

impl HostConfig {
    pub fn validate(self) -> anyhow::Result<ValidatedHostConfig> {
        let workspace = self.workspace.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "workspace does not exist or is inaccessible: {} ({error})",
                self.workspace.display()
            )
        })?;
        let provider = reqwest::Url::parse(&self.base_url)?;
        if provider.scheme() != "https" || provider.host_str().is_none() {
            anyhow::bail!("provider base_url must be an https URL with a host");
        }
        if self.model.trim().is_empty() {
            anyhow::bail!("model must not be blank");
        }
        let plugin_root = self
            .plugin_root
            .map(|root| {
                root.canonicalize().map_err(|error| {
                    anyhow::anyhow!(
                        "plugin root does not exist or is inaccessible: {} ({error})",
                        root.display()
                    )
                })
            })
            .transpose()?;
        Ok(ValidatedHostConfig {
            base_url: self.base_url,
            model: self.model,
            fetch_allow: self.fetch_allow,
            workspace,
            plugin_root,
        })
    }
}

pub struct ProductionHost {
    config: ValidatedHostConfig,
    proxy: ProviderProxy,
    proxy_events: Arc<Mutex<Vec<Event>>>,
    client: OpenAiCompatClient,
    registry: ToolRegistry,
    cancel_token: CancellationToken,
    result_middleware: ProductionResultMiddleware,
}

impl ProductionHost {
    pub fn start(
        config: HostConfig,
        approver: Option<Arc<dyn Approver + Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let key = std::env::var(model_provider::API_KEY_ENV).map_err(|_| {
            anyhow::anyhow!(
                "环境变量 {} 未设置；拒绝以匿名方式调用上游",
                model_provider::API_KEY_ENV
            )
        })?;
        Self::start_with_api_key(config, key, approver)
    }

    /// Desktop-safe constructor: the caller obtains the key from an OS credential store and the
    /// host never writes it to configuration, events, or command responses.
    pub fn start_with_api_key(
        config: HostConfig,
        api_key: impl Into<String>,
        approver: Option<Arc<dyn Approver + Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let config = config.validate()?;
        let proxy_events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let proxy = ProviderProxy::start_with_fetch_hosts(
            &config.base_url,
            &config.fetch_allow,
            Arc::clone(&proxy_events),
        )?;
        let client = OpenAiCompatClient::with_key_via_proxy(api_key, &proxy.url)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let cancel_token = CancellationToken::default();
        let mut registry = ToolRegistry::default();
        register_chat_tools(
            &mut registry,
            &config.workspace,
            config.plugin_root.as_deref(),
            &config.fetch_allow,
            &proxy.url,
            &cancel_token,
        )?;
        registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
        register_exec_tool(&mut registry, PlatformRestrictedBackend, approver);
        Ok(Self {
            config,
            proxy,
            proxy_events,
            client,
            registry,
            cancel_token,
            result_middleware: ProductionResultMiddleware::default(),
        })
    }

    pub fn probe_provider(&self) -> Result<(), ProviderProbeFailure> {
        self.client.probe(&self.config.base_url)
    }

    pub fn config(&self) -> &ValidatedHostConfig {
        &self.config
    }

    pub fn client(&self) -> &OpenAiCompatClient {
        &self.client
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn model_spec(&self) -> ModelCallSpec {
        ModelCallSpec {
            model: self.config.model.clone(),
            base_url: self.config.base_url.clone(),
            temperature: None,
        }
    }

    pub fn tool_definitions(&self) -> Result<serde_json::Value, ErrorEnvelope> {
        let names: Vec<&str> = self.registry.names().collect();
        openai_tools_json(&self.registry, &names)
    }

    pub fn result_middleware(&self) -> &ProductionResultMiddleware {
        &self.result_middleware
    }

    pub fn build_agent_loop<'a>(
        &'a self,
        session: &'a mut dyn SessionStore,
        history: Vec<ChatMessage>,
        external_events: &'a dyn Fn() -> Vec<Event>,
    ) -> Result<AgentLoop<'a>, ErrorEnvelope> {
        let mut agent =
            AgentLoop::with_chat(session, &self.registry, &self.client, self.model_spec());
        agent.result_middleware = Some(&self.result_middleware);
        agent.loop_guard = Some(LoopGuard {
            remind_after: DEFAULT_LOOP_REMIND_AFTER,
            reject_after: DEFAULT_LOOP_REJECT_AFTER,
        });
        agent.tool_definitions = Some(self.tool_definitions()?);
        agent.chat_history = history;
        agent.mark_queued_inputs_consumed();
        agent.external_events = Some(external_events);
        Ok(agent)
    }

    pub fn proxy_event_queue(&self) -> Arc<Mutex<Vec<Event>>> {
        Arc::clone(&self.proxy_events)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.cancel_token.cancel();
        self.proxy.shutdown()
    }
}

pub struct PreparedSession {
    pub session: JsonlSession,
    pub history: Vec<ChatMessage>,
    pub recovered_dangling_turn: bool,
    pub memories_injected: bool,
}

pub fn prepare_session(path: impl Into<PathBuf>) -> anyhow::Result<PreparedSession> {
    let mut session = JsonlSession::create(path.into())?;
    let recovered_dangling_turn = recover_dangling_turn(&mut session)?;
    let memories_injected = inject_memories(&mut session)?;
    let history = rebuild_history(session.path())?;
    Ok(PreparedSession {
        session,
        history,
        recovered_dangling_turn,
        memories_injected,
    })
}

pub struct ProductionResultMiddleware {
    max_result_bytes: usize,
}

impl Default for ProductionResultMiddleware {
    fn default() -> Self {
        Self {
            max_result_bytes: DEFAULT_RESULT_BYTES,
        }
    }
}

impl ProductionResultMiddleware {
    pub fn with_max_result_bytes(max_result_bytes: usize) -> anyhow::Result<Self> {
        if max_result_bytes == 0 {
            anyhow::bail!("max_result_bytes must be greater than zero");
        }
        Ok(Self { max_result_bytes })
    }
}

impl ToolResultMiddleware for ProductionResultMiddleware {
    fn inspect(
        &self,
        context: &ToolResultContext<'_>,
    ) -> Result<ToolResultDecision, ErrorEnvelope> {
        match context.outcome {
            ToolOutcome::Success { value } => {
                let serialized = serde_json::to_string(value).unwrap_or_default();
                if serialized.len() > self.max_result_bytes {
                    let preview: String = serialized.chars().take(2_000).collect();
                    return Ok(ToolResultDecision::Redact(ToolOutcome::Success {
                        value: serde_json::json!({
                            "truncated_by_result_guard": true,
                            "original_bytes": serialized.len(),
                            "preview": preview,
                        }),
                    }));
                }
                Ok(ToolResultDecision::Allow)
            }
            ToolOutcome::Failure { .. } => Ok(ToolResultDecision::Allow),
        }
    }
}

pub fn register_chat_tools(
    registry: &mut ToolRegistry,
    workspace: &Path,
    plugin_root: Option<&Path>,
    fetch_hosts: &[String],
    proxy_url: &str,
    cancel_token: &CancellationToken,
) -> anyhow::Result<usize> {
    let fs_tools =
        tools::FsToolSet::new(workspace).map_err(|error| anyhow::anyhow!(error.message))?;
    fs_tools.set_cancellation_token(cancel_token.clone());
    registry.set_cancellation_token(Arc::new(cancel_token.clone()));
    fs_tools.register(registry);
    register_demo_tools(registry);
    register_web_fetch_tool(registry, proxy_url, fetch_hosts)?;
    register_memory_tool(registry);

    let mut plugin_tools = 0;
    if let Some(root) = plugin_root {
        let catalog = Arc::new(
            tools::PluginCatalog::discover_explicit(root)
                .map_err(|error| anyhow::anyhow!(error.message))?,
        );
        for failure in catalog.failures() {
            eprintln!(
                "  ⚠ 插件 {} 被隔离（{:?}）：{}",
                failure.plugin, failure.stage, failure.error.message
            );
        }
        for plugin in catalog.plugins() {
            match catalog.bind_static_tools(registry, plugin.name()) {
                Ok(count) => {
                    println!(
                        "  插件已装配: {} v{}（+{count} 工具）",
                        plugin.name(),
                        plugin.version()
                    );
                    plugin_tools += count;
                }
                Err(error) => eprintln!(
                    "  ⚠ 插件 {} 绑定失败（已跳过，不遮蔽其他插件）：{}",
                    plugin.name(),
                    error.message
                ),
            }
        }
    }
    Ok(plugin_tools)
}

struct ProxiedHttpFetcher {
    proxy_url: String,
}

impl tools::Fetcher for ProxiedHttpFetcher {
    fn fetch(&self, request: &tools::FetchRequest) -> Result<tools::FetchResponse, ErrorEnvelope> {
        let outcome = model_provider::http_fetch_via_proxy(
            Some(&self.proxy_url),
            &request.url,
            request.max_bytes,
            std::time::Duration::from_secs(30),
        )?;
        Ok(tools::FetchResponse {
            status: outcome.status,
            location: outcome.location,
            body: outcome.body,
            truncated: outcome.truncated,
        })
    }
}

fn register_web_fetch_tool(
    registry: &mut ToolRegistry,
    proxy_url: &str,
    allowed_hosts: &[String],
) -> anyhow::Result<()> {
    let fetcher: Arc<dyn tools::Fetcher> = Arc::new(ProxiedHttpFetcher {
        proxy_url: proxy_url.to_string(),
    });
    let spill_root = std::env::current_dir()?.join(".harness").join("spill");
    let tool = tools::WebFetchTool::new(
        fetcher,
        allowed_hosts.iter().cloned().collect(),
        spill_root,
        ".harness/spill",
        1024 * 1024,
    );
    registry.register(
        ToolSpec {
            name: "web_fetch".into(),
            description:
                "抓取白名单内主机的 http(s) 页面文本；仅经本地白名单代理出网，私网/回环一律拒绝"
                    .into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": { "url": { "type": "string" } }
            }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(move |args| tool.fetch(args)),
    );
    Ok(())
}

pub fn register_memory_tool(registry: &mut ToolRegistry) {
    registry.register_audited(
        ToolSpec {
            name: "memory_write".into(),
            description: "把一条跨会话记忆事实写入事件流（会随 resume/fork 重放恢复）".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(|args| {
            let Some(text) = args["text"].as_str().map(str::to_owned) else {
                return ToolExecution::new(ToolOutcome::Failure {
                    error: protocol::ErrorEnvelope::new(
                        protocol::ErrorCode::ToolArgsInvalid,
                        "missing string argument: text",
                    ),
                });
            };
            if text.trim().is_empty() {
                return ToolExecution::new(ToolOutcome::Failure {
                    error: protocol::ErrorEnvelope::new(
                        protocol::ErrorCode::ToolArgsInvalid,
                        "memory text must not be empty",
                    ),
                });
            }
            if let Err(error) = session::validate_memory_size(&text) {
                return ToolExecution::new(ToolOutcome::Failure { error });
            }
            let tags = args["tags"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            ToolExecution {
                outcome: ToolOutcome::Success {
                    value: serde_json::json!({ "recorded": true, "text": text }),
                },
                audits: vec![ToolAudit::MemoryRecorded {
                    text,
                    tags,
                    source: protocol::MemorySource::Model,
                    scope: protocol::MemoryScope::LineageOnly,
                }],
            }
        }),
    );
}

fn register_demo_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolSpec {
            name: "echo".into(),
            description: "回声工具：原样返回 text 参数".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(|args| ToolOutcome::Success {
            value: serde_json::json!({ "echoed": args["text"] }),
        }),
    );
    registry.register(
        ToolSpec {
            name: "now".into(),
            description: "返回当前 Unix 时间戳（秒）".into(),
            parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
            escalation_capable: false,
            timeout_ms: None,
        },
        Box::new(|_| ToolOutcome::Success {
            value: serde_json::json!({
                "unix_seconds": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0)
            }),
        }),
    );
}

pub fn openai_tools_json(
    registry: &ToolRegistry,
    names: &[&str],
) -> Result<serde_json::Value, ErrorEnvelope> {
    Ok(serde_json::Value::Array(
        names
            .iter()
            .filter_map(|name| registry.get(name))
            .map(|spec| {
                Ok(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.advertised_parameters_schema(
                            registry.escalation_availability()
                        )?,
                    },
                }))
            })
            .collect::<Result<Vec<_>, ErrorEnvelope>>()?,
    ))
}

pub fn inject_memories(session: &mut dyn SessionStore) -> anyhow::Result<bool> {
    let events = session.replay_events()?;
    let memories = session::project_memories(&events)?;
    if memories.is_empty() {
        return Ok(false);
    }
    let summary =
        session::injection_summary(&memories).map_err(|error| anyhow::anyhow!(error.message))?;
    let already = events.iter().any(|sequenced| {
        matches!(
            &sequenced.event,
            Event::MemoryContextInjected { summary: existing } if *existing == summary
        )
    });
    if already {
        return Ok(false);
    }
    session.append(Event::MemoryContextInjected { summary })?;
    Ok(true)
}

pub fn recover_dangling_turn(session: &mut dyn SessionStore) -> anyhow::Result<bool> {
    let events = replay_session(session.path())?;
    let Some(last_start) = events
        .iter()
        .rev()
        .find_map(|sequenced| match sequenced.event {
            Event::TurnStarted { turn_id } => Some(turn_id),
            _ => None,
        })
    else {
        return Ok(false);
    };
    let finished = events.iter().any(|sequenced| {
        matches!(
            &sequenced.event,
            Event::TurnCompleted { turn_id } | Event::TurnAborted { turn_id, .. }
                if *turn_id == last_start
        )
    });
    if finished {
        return Ok(false);
    }
    session.append(Event::TurnAborted {
        turn_id: last_start,
        reason: "interrupted: session reopened".into(),
    })?;
    Ok(true)
}

pub fn rebuild_history(path: &Path) -> anyhow::Result<Vec<ChatMessage>> {
    session::project_model_surface(&replay_session(path)?)
        .map_err(|error| {
            anyhow::anyhow!(
                "model surface projection failed ({:?}): {}",
                error.code,
                error.message
            )
        })?
        .into_iter()
        .map(|entry| ChatMessage::try_from(entry.message).map_err(anyhow::Error::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-host-{}-{name}", std::process::id()))
    }

    #[test]
    fn config_validation_canonicalizes_workspace_and_rejects_bad_boundaries() {
        let workspace = temporary_workspace("config");
        std::fs::create_dir_all(&workspace).unwrap();
        let validated = HostConfig {
            workspace: workspace.clone(),
            ..HostConfig::default()
        }
        .validate()
        .unwrap();
        assert_eq!(validated.workspace, workspace.canonicalize().unwrap());

        let bad_url = HostConfig {
            base_url: "http://example.com/v1".into(),
            workspace: workspace.clone(),
            ..HostConfig::default()
        };
        assert!(bad_url.validate().is_err());
        let missing_workspace = HostConfig {
            workspace: workspace.join("missing"),
            ..HostConfig::default()
        };
        assert!(missing_workspace.validate().is_err());
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn result_middleware_rejects_zero_budget() {
        assert!(ProductionResultMiddleware::with_max_result_bytes(0).is_err());
    }
}
