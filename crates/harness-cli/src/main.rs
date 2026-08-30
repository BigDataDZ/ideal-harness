//! 装配入口：`ideal-harness`（最小演示）/ `ideal-harness chat`（可对话 MVP，TASK-104）。
//! chat 模式 = 真实模型 + 工具调用闭环 + JSONL 会话持久化 + 中断恢复。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_loop::{AgentLoop, ModelProvider};
use approval::TerminalApprover;
use model_provider::{ChatMessage, OpenAiCompatClient};
use protocol::{ErrorCode, ErrorEnvelope, Event, ModelCallSpec, ToolOutcome};
use sandbox_exec::PlatformRestrictedBackend;
use sandbox_policy::{SandboxMode, SandboxPolicy};
use session::{replay_session, JsonlSession, SessionStore};
use tools::{EscalationAvailability, ToolRegistry, ToolSpec};

mod readonly_server;
#[cfg(test)]
mod scenario_snapshots;
mod security;
mod session_commands;
use readonly_server::cmd_serve;
use security::{register_exec_tool, ProviderProxy};
use session_commands::{cmd_fork, cmd_resume, cmd_revert, cmd_timeline};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("chat") => cmd_chat(&args[1..]),
        Some("resume") => cmd_resume(&args[1..]),
        Some("fork") => cmd_fork(&args[1..]),
        Some("timeline") => cmd_timeline(&args[1..]),
        Some("revert") => cmd_revert(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
        _ => demo(),
    }
}

// ---------------------------------------------------------------------------
// chat 子命令（TASK-104）
// ---------------------------------------------------------------------------

struct ChatArgs {
    session: PathBuf,
    base_url: String,
    model: String,
    /// TASK-703：web_fetch 允许的主机白名单；默认空 = 全部拒绝（fail-closed）。
    fetch_allow: Vec<String>,
}

impl Default for ChatArgs {
    fn default() -> Self {
        Self {
            session: std::env::temp_dir().join("ideal-harness-chat.jsonl"),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            fetch_allow: Vec::new(),
        }
    }
}

fn next_value(args: &[String], i: usize, flag: &str) -> anyhow::Result<String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} 缺少取值"))
}

fn parse_chat_args(args: &[String]) -> anyhow::Result<ChatArgs> {
    let mut out = ChatArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => out.session = PathBuf::from(next_value(args, i, "--session")?),
            "--base-url" => out.base_url = next_value(args, i, "--base-url")?,
            "--model" => out.model = next_value(args, i, "--model")?,
            "--fetch-allow" => out.fetch_allow.push(next_value(args, i, "--fetch-allow")?),
            other => anyhow::bail!(
                "未知参数 {other}；用法: ideal-harness chat [--session <path>] [--base-url <url>] [--model <name>]"
            ),
        }
        i += 2;
    }
    Ok(out)
}

fn cmd_chat(args: &[String]) -> anyhow::Result<()> {
    let cfg = parse_chat_args(args)?;
    let proxy_events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let mut proxy = ProviderProxy::start(&cfg.base_url, Arc::clone(&proxy_events))?;
    // fail-closed：无 key 直接拒绝启动（红线 3），绝不匿名调用上游。
    let client = OpenAiCompatClient::from_env_via_proxy(&proxy.url).map_err(|e| {
        anyhow::anyhow!(
            "{}（请先设置环境变量 IDEAL_HARNESS_API_KEY 后重试）",
            e.message
        )
    })?;
    let spec = ModelCallSpec {
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        temperature: None,
    };

    let mut registry = ToolRegistry::default();
    register_demo_tools(&mut registry);
    register_web_fetch_tool(&mut registry, &proxy.url, &cfg.fetch_allow)?;
    registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
    let terminal_approver = Arc::new(TerminalApprover::new(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stderr(),
    ));
    register_exec_tool(
        &mut registry,
        PlatformRestrictedBackend,
        Some(terminal_approver),
    );

    // 会话复用：create 即 append 模式，seq 自动续接（崩溃安全）。
    let mut js = JsonlSession::create(cfg.session.clone())?;
    if recover_dangling_turn(&mut js)? {
        println!("（检测到上次中断的 turn，已补记 TurnAborted——事件溯源崩溃恢复）");
    }
    let history = rebuild_history(js.path())?;

    let mut lp = AgentLoop::with_chat(&mut js, &registry, &client, spec);
    lp.tool_definitions = Some(
        openai_tools_json(&registry, &["echo", "now", "web_fetch", "exec"])
            .map_err(|error| anyhow::anyhow!(error.message))?,
    );
    lp.chat_history = history;
    let event_source_queue = Arc::clone(&proxy_events);
    let event_source = move || match event_source_queue.lock() {
        Ok(mut events) => std::mem::take(&mut *events),
        Err(_) => Vec::new(),
    };
    lp.external_events = Some(&event_source);

    println!("== ideal-harness chat ==");
    println!(
        "模型 {} @ {} | 会话 {}",
        cfg.model,
        cfg.base_url,
        lp.session.path().display()
    );
    println!("直接输入即对话；/tools 查看工具；/exit 退出（EOF 同效）；Ctrl+C 直接退出，下次启动自动补记中断");

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF（Windows Ctrl+Z 回车 / Unix Ctrl+D）
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match text {
            "/exit" | "/quit" => break,
            "/tools" => {
                for name in ["echo", "now", "web_fetch", "exec"] {
                    if let Some(s) = registry.get(name) {
                        println!("  {} — {}", s.name, s.description);
                    }
                }
                continue;
            }
            _ => {}
        }

        lp.inbox.push(text);
        let before = lp.session.len();
        lp.run_turn();
        // 一切留痕（红线 5）：把本 turn 新增事件投影到终端。
        for se in replay_session(lp.session.path())?
            .into_iter()
            .skip(before as usize)
        {
            match se.event {
                Event::ToolCallRequested { tool, args, .. } => {
                    println!("  ⚙ 调用 {tool}({args})");
                }
                Event::AssistantMessage { text } => println!("assistant: {text}"),
                Event::TurnAborted { reason, .. } => println!("  ⚠ turn 中止：{reason}"),
                _ => {}
            }
        }
    }
    println!("会话已保存：{}", lp.session.path().display());
    drop(lp);
    drop(client);
    proxy.shutdown()?;
    Ok(())
}

/// TASK-703：把物理出网接到本地 CONNECT 白名单代理的 Fetcher 适配器。
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

/// TASK-703：注册 web_fetch 工具（主机默认拒绝，`--fetch-allow` 显式放行）。
fn register_web_fetch_tool(
    registry: &mut ToolRegistry,
    proxy_url: &str,
    allowed_hosts: &[String],
) -> anyhow::Result<()> {
    let fetcher: std::sync::Arc<dyn tools::Fetcher> = std::sync::Arc::new(ProxiedHttpFetcher {
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
        },
        Box::new(move |args| tool.fetch(args)),
    );
    Ok(())
}

/// 演示工具集：echo（自纠演示）+ now（真实副作用最小示例）。
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
        },
        Box::new(|_| ToolOutcome::Success {
            value: serde_json::json!({
                "unix_seconds": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }),
    );
}

/// 把注册表中的指定工具包装为 OpenAI tools 数组（chat 请求的 tools 广告）。
fn openai_tools_json(
    registry: &ToolRegistry,
    names: &[&str],
) -> Result<serde_json::Value, ErrorEnvelope> {
    Ok(serde_json::Value::Array(
        names
            .iter()
            .filter_map(|n| registry.get(n))
            .map(|s| {
                Ok(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": s.name,
                        "description": s.description,
                        "parameters": s.advertised_parameters_schema(
                            registry.escalation_availability()
                        )?,
                    },
                }))
            })
            .collect::<Result<Vec<_>, ErrorEnvelope>>()?,
    ))
}

/// 事件溯源原生的中断恢复：悬空 turn（有 Started、无 Completed/Aborted）
/// 补记 TurnAborted——Ctrl+C 硬退出的下次启动自动收口（红线 5：一切留痕）。
fn recover_dangling_turn(session: &mut dyn SessionStore) -> anyhow::Result<bool> {
    let events = replay_session(session.path())?;
    let Some(last_start) = events.iter().rev().find_map(|se| match se.event {
        Event::TurnStarted { turn_id } => Some(turn_id),
        _ => None,
    }) else {
        return Ok(false);
    };
    let finished = events.iter().any(|se| {
        matches!(
            &se.event,
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

/// TASK-601：从唯一事件流投影完整模型可见历史；审计事件不会混入上下文。
fn rebuild_history(path: &Path) -> anyhow::Result<Vec<ChatMessage>> {
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

// ---------------------------------------------------------------------------
// 最小演示（v0.1 保留）：沙箱语义 / 工具自纠 / 事件回放
// ---------------------------------------------------------------------------

struct EchoProvider;

impl ModelProvider for EchoProvider {
    fn complete(&self, _user_text: &str) -> Result<String, protocol::ErrorEnvelope> {
        Ok("echo: 收到".into())
    }
}

fn demo() -> anyhow::Result<()> {
    println!("== ideal-harness 原型演示 ==");

    // 1) 沙箱策略（P2）
    let policy = SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: std::env::temp_dir(),
    };
    let outside = std::path::Path::new("C:/Windows/system32/config.sys");
    println!(
        "sandbox mode = {:?}; write {:?} allowed? {}",
        policy.mode,
        outside,
        policy.ensures_writable(outside)
    );

    // 2) 工具注册与调度（P3）：合法调用 vs 缺参自纠
    let mut registry = ToolRegistry::default();
    register_demo_tools(&mut registry);
    let ok = registry.dispatch("echo", &serde_json::json!({ "text": "hello harness" }));
    let bad = registry.dispatch("echo", &serde_json::json!({}));
    println!("dispatch ok  -> {ok:?}");
    println!("dispatch bad -> {bad:?} (ToolArgsInvalid 回传给模型自纠)");

    // 3) Agent 循环 + 事件溯源（P5/P7）
    let path = std::env::temp_dir().join("ideal-harness-demo.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut js = JsonlSession::create(path.clone())?;

    let mut lp = AgentLoop::new(&mut js, &registry, &EchoProvider);
    lp.inbox.push("你好，harness");
    let n = lp.run_turn();
    println!("turn completed exchanges = {n}");

    println!("---- 事件流回放 ----");
    for se in replay_session(&path)? {
        println!("{:>3}  {}", se.seq, serde_json::to_string(&se.event)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-cli-{}-{name}", std::process::id()))
    }

    #[test]
    fn parse_chat_args_defaults_and_overrides() {
        let parsed = parse_chat_args(&[]).unwrap();
        assert_eq!(parsed.model, "deepseek-chat");
        assert!(parsed.base_url.contains("deepseek"));

        let parsed = parse_chat_args(&[
            "--session".into(),
            "s.jsonl".into(),
            "--base-url".into(),
            "http://127.0.0.1:9".into(),
            "--model".into(),
            "m1".into(),
        ])
        .unwrap();
        assert_eq!(parsed.session, PathBuf::from("s.jsonl"));
        assert_eq!(parsed.base_url, "http://127.0.0.1:9");
        assert_eq!(parsed.model, "m1");

        assert!(parse_chat_args(&["--model".into()]).is_err());
        assert!(parse_chat_args(&["--wat".into()]).is_err());
    }

    #[test]
    fn dangling_turn_recovered_on_reopen_then_noop() {
        let path = tmp("dangling.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        js.append(Event::TurnStarted { turn_id: 0 }).unwrap();
        js.append(Event::UserMessage { text: "hi".into() }).unwrap();
        // 模拟 Ctrl+C 硬退出后重开：没有 Completed/Aborted 终态
        assert!(
            recover_dangling_turn(&mut js).unwrap(),
            "悬空 turn 必须被补记"
        );
        let evs = replay_session(&path).unwrap();
        match &evs.last().unwrap().event {
            Event::TurnAborted { turn_id, reason } => {
                assert_eq!(*turn_id, 0);
                assert!(reason.contains("interrupted"));
            }
            other => panic!("expected turn_aborted, got {other:?}"),
        }
        // 第二次：已收口，不再补记
        assert!(!recover_dangling_turn(&mut js).unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn finished_turn_not_recovered() {
        let path = tmp("finished.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        js.append(Event::TurnStarted { turn_id: 1 }).unwrap();
        js.append(Event::TurnCompleted { turn_id: 1 }).unwrap();
        assert!(!recover_dangling_turn(&mut js).unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rebuild_history_ignores_non_surface_stream_events() {
        let path = tmp("rebuild.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut js = JsonlSession::create(path.clone()).unwrap();
            js.append(Event::TurnStarted { turn_id: 0 }).unwrap();
            js.append(Event::UserMessage { text: "q".into() }).unwrap();
            js.append(Event::ModelChunkReceived {
                call_id: "c".into(),
                delta_text: "par".into(),
            })
            .unwrap();
            js.append(Event::AssistantMessage { text: "a".into() })
                .unwrap();
            js.append(Event::AssistantMessage {
                text: String::new(),
            })
            .unwrap(); // 空文本不入历史
        }
        let h = rebuild_history(&path).unwrap();
        assert_eq!(h, vec![ChatMessage::user("q"), ChatMessage::assistant("a")]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rebuild_history_restores_tools_and_exact_compaction_without_audit_hooks() {
        let path = tmp("rebuild-tools.jsonl");
        let _ = std::fs::remove_file(&path);
        let model_outcome = ToolOutcome::Success {
            value: serde_json::json!("ok"),
        };
        {
            let mut js = JsonlSession::create(path.clone()).unwrap();
            js.append(Event::UserMessage { text: "old".into() })
                .unwrap();
            js.append(Event::AssistantMessage {
                text: "old answer".into(),
            })
            .unwrap();
            js.append(Event::UserMessage { text: "q".into() }).unwrap();
            js.append(Event::ModelToolCallsRequested {
                request_id: "r1".into(),
                calls: vec![protocol::ModelToolCall {
                    id: "c1".into(),
                    name: "lookup".into(),
                    arguments: r#"{"q":"rust"}"#.into(),
                }],
            })
            .unwrap();
            js.append(Event::ToolCallRequested {
                call_id: "c1".into(),
                tool: "lookup".into(),
                args: serde_json::json!({"q":"rust"}),
            })
            .unwrap();
            js.append(Event::ToolResultAdded {
                call_id: "c1".into(),
                outcome: model_outcome.clone(),
            })
            .unwrap();
            js.append(Event::ToolCallRequested {
                call_id: "hook-1".into(),
                tool: "hook:audit".into(),
                args: serde_json::json!({}),
            })
            .unwrap();
            js.append(Event::ToolResultAdded {
                call_id: "hook-1".into(),
                outcome: ToolOutcome::Success {
                    value: serde_json::Value::Null,
                },
            })
            .unwrap();
            js.append(Event::AssistantMessage {
                text: "done".into(),
            })
            .unwrap();
            js.append(Event::CompactionApplied {
                summary: "older context".into(),
                compacted_messages: Some(2),
                source_event_seqs: vec![0, 1],
            })
            .unwrap();
        }

        let history = rebuild_history(&path).unwrap();
        assert_eq!(history.len(), 5);
        assert_eq!(
            history[0],
            ChatMessage::system("Compacted conversation summary:\nolder context")
        );
        assert_eq!(history[1], ChatMessage::user("q"));
        assert_eq!(history[2].tool_calls.as_ref().unwrap()[0].id, "c1");
        assert_eq!(
            history[3],
            ChatMessage::tool_result("c1", serde_json::to_string(&model_outcome).unwrap())
        );
        assert_eq!(history[4], ChatMessage::assistant("done"));
        assert!(history
            .iter()
            .all(|message| !message.content.contains("hook:audit")));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn openai_tools_json_wraps_registered_specs() {
        let mut reg = ToolRegistry::default();
        register_demo_tools(&mut reg);
        let v = openai_tools_json(&reg, &["echo", "now"]).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "echo");
        assert_eq!(v[1]["function"]["name"], "now");
    }

    struct CannedFetcher;
    impl tools::Fetcher for CannedFetcher {
        fn fetch(&self, _: &tools::FetchRequest) -> Result<tools::FetchResponse, ErrorEnvelope> {
            Ok(tools::FetchResponse {
                status: 200,
                location: None,
                body: b"hello via proxy".to_vec(),
                truncated: false,
            })
        }
    }

    /// TASK-703 装配冒烟：默认拒绝 + 白名单放行 + 代理通道断言不可绕过。
    #[test]
    fn web_fetch_assembly_denies_by_default_and_serves_allowlisted_host() {
        let spill = tmp("web-spill");
        let _ = std::fs::remove_dir_all(&spill);
        let tool = tools::WebFetchTool::new(
            std::sync::Arc::new(CannedFetcher),
            ["docs.example".to_string()].into_iter().collect(),
            spill.join(".harness").join("spill"),
            ".harness/spill",
            64 * 1024,
        );
        let mut registry = ToolRegistry::default();
        registry.register(
            ToolSpec {
                name: "web_fetch".into(),
                description: "demo".into(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "required": ["url"],
                    "properties": { "url": { "type": "string" } }
                }),
                escalation_capable: false,
            },
            Box::new(move |args| tool.fetch(args)),
        );
        // 未在白名单内的主机 fail-closed（空策略也安全）
        let denied = registry
            .dispatch(
                "web_fetch",
                &serde_json::json!({ "url": "https://evil.example/x" }),
            )
            .unwrap();
        match denied {
            ToolOutcome::Failure { error } => {
                assert_eq!(error.code, ErrorCode::SandboxDenied);
                assert!(error.message.contains("not allowlisted"));
            }
            other => panic!("expected denial, got {other:?}"),
        }
        let allowed = registry
            .dispatch(
                "web_fetch",
                &serde_json::json!({ "url": "https://docs.example/x" }),
            )
            .unwrap();
        match allowed {
            ToolOutcome::Success { value } => assert_eq!(value["content"], "hello via proxy"),
            other => panic!("expected fetch success, got {other:?}"),
        }
        std::fs::remove_dir_all(&spill).ok();
    }

    #[test]
    fn parse_chat_args_collects_fetch_allow_hosts() {
        let parsed = parse_chat_args(&[
            "--fetch-allow".into(),
            "docs.example".into(),
            "--fetch-allow".into(),
            "wiki.example".into(),
        ])
        .unwrap();
        assert_eq!(parsed.fetch_allow, vec!["docs.example", "wiki.example"]);
    }

    /// TASK-701 装配冒烟：工作区 → FsToolSet 注册 → read/write/edit 闭环。
    #[test]
    fn fs_toolset_assembly_read_write_edit_roundtrip() {
        let root = tmp("fs-assembly");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let set = tools::FsToolSet::new(&root).unwrap();
        let mut registry = ToolRegistry::default();
        set.register(&mut registry);
        registry
            .dispatch(
                "fs_write",
                &serde_json::json!({ "path": "note.txt", "content": "one\n" }),
            )
            .expect("new file write must succeed");
        registry
            .dispatch("fs_read", &serde_json::json!({ "path": "note.txt" }))
            .expect("read back must succeed");
        let edit = registry
            .dispatch(
                "fs_edit",
                &serde_json::json!({ "path": "note.txt", "old_string": "one", "new_string": "two" }),
            )
            .expect("edit after read must succeed");
        match edit {
            ToolOutcome::Success { value } => assert_eq!(value["replacements"], 1),
            other => panic!("expected edit success, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "two\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// TASK-607 装配冒烟：workspace → 插件目录发现 → 绑定 → 调度回 payload。
    #[test]
    fn plugin_assembly_discovers_binds_and_dispatches_payload() {
        let root = tmp("plugin-assembly");
        let _ = std::fs::remove_dir_all(&root);
        let payload = r#"{"greeting":"hi from plugin"}"#;
        let dir = root.join(".harness/plugins/greeter");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({
            "name": "greeter",
            "version": "1.0.0",
            "payload": "payload.json",
            "hash": tools::content_hash(payload.as_bytes()),
            "tools": [{
                "name": "greeter_hello",
                "description": "Greet",
                "parameters_schema": { "type": "object", "properties": {} }
            }]
        })
        .to_string();
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        std::fs::write(dir.join("payload.json"), payload).unwrap();

        let catalog = std::sync::Arc::new(tools::PluginCatalog::discover(&root).unwrap());
        assert!(catalog.failures().is_empty());
        let mut registry = ToolRegistry::default();
        assert_eq!(
            catalog.bind_static_tools(&mut registry, "greeter").unwrap(),
            1
        );
        assert_eq!(registry.plugin_provenance("greeter_hello"), Some("greeter"));
        match registry.dispatch("greeter_hello", &serde_json::json!({})) {
            Some(ToolOutcome::Success { value }) => {
                assert_eq!(value["greeting"], "hi from plugin")
            }
            other => panic!("expected payload result, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
