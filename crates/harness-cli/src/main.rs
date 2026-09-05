//! 装配入口：`ideal-harness`（最小演示）/ `ideal-harness chat`（可对话 MVP，TASK-104）。
//! chat 模式 = 真实模型 + 工具调用闭环 + JSONL 会话持久化 + 中断恢复。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use agent_loop::{AgentLoop, ModelProvider};
use approval::TerminalApprover;
use harness_host::{prepare_session, HostConfig, PreparedSession, ProductionHost};
use protocol::{Event, ToolOutcome};
use sandbox_policy::{SandboxMode, SandboxPolicy};
use session::{replay_session, JsonlSession};
use tools::{ToolRegistry, ToolSpec};

#[cfg(test)]
use agent_loop::{LoopGuard, ToolResultContext, ToolResultDecision, ToolResultMiddleware};
#[cfg(test)]
use harness_host::{
    inject_memories, openai_tools_json, rebuild_history, recover_dangling_turn,
    register_chat_tools, register_exec_tool, register_memory_tool, ProductionResultMiddleware,
};
#[cfg(test)]
use model_provider::ChatMessage;
#[cfg(test)]
use protocol::ErrorEnvelope;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use tools::EscalationAvailability;

mod readonly_server;
#[cfg(test)]
mod scenario_snapshots;
mod session_commands;
use readonly_server::cmd_serve;
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
    /// TASK-803：文件工具与相对路径的信任根；默认当前目录。
    workspace: PathBuf,
    /// TASK-803：显式插件根目录；None = 不加载任何插件（不自动信任工作区插件）。
    plugin_root: Option<PathBuf>,
}

impl Default for ChatArgs {
    fn default() -> Self {
        Self {
            session: std::env::temp_dir().join("ideal-harness-chat.jsonl"),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            fetch_allow: Vec::new(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            plugin_root: None,
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
            "--workspace" => out.workspace = PathBuf::from(next_value(args, i, "--workspace")?),
            "--plugin-root" => out.plugin_root = Some(PathBuf::from(next_value(args, i, "--plugin-root")?)),
            other => anyhow::bail!(
                "未知参数 {other}；用法: ideal-harness chat [--session <path>] [--base-url <url>] [--model <name>] [--workspace <dir>] [--plugin-root <dir>] [--fetch-allow <host>]"
            ),
        }
        i += 2;
    }
    Ok(out)
}

fn cmd_chat(args: &[String]) -> anyhow::Result<()> {
    let cfg = parse_chat_args(args)?;
    let terminal_approver = Arc::new(TerminalApprover::new(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stderr(),
    ));
    let host = ProductionHost::start(
        HostConfig {
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            fetch_allow: cfg.fetch_allow.clone(),
            workspace: cfg.workspace.clone(),
            plugin_root: cfg.plugin_root.clone(),
        },
        Some(terminal_approver),
    )?;
    let PreparedSession {
        mut session,
        history,
        recovered_dangling_turn,
        memories_injected,
    } = prepare_session(cfg.session.clone())?;
    if recovered_dangling_turn {
        println!("（检测到上次中断的 turn，已补记 TurnAborted——事件溯源崩溃恢复）");
    }
    if memories_injected {
        println!("（已注入跨会话记忆）");
    }
    let proxy_events = host.proxy_event_queue();
    let event_source = move || match proxy_events.lock() {
        Ok(mut events) => std::mem::take(&mut *events),
        Err(_) => Vec::new(),
    };
    let mut lp = host
        .build_agent_loop(&mut session, history, &event_source)
        .map_err(|error| anyhow::anyhow!(error.message))?;

    println!("== ideal-harness chat ==");
    println!(
        "模型 {} @ {} | 会话 {}",
        cfg.model,
        cfg.base_url,
        lp.session.path().display()
    );
    println!(
        "直接输入即对话；/tools 查看工具；/steer <文本> 跨轮插入输入；/exit 退出（EOF 同效）；Ctrl+C 直接退出，下次启动自动补记中断",
    );

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
                for name in lp.tools.names() {
                    if let Some(s) = lp.tools.get(name) {
                        println!("  {} — {}", s.name, s.description);
                    }
                }
                continue;
            }
            t if t.starts_with("/steer") => {
                // TASK-803/704：steer 输入事件化入队，在下一采样轮边界被吸收
                let payload = t.trim_start_matches("/steer").trim();
                match lp.enqueue_input(payload) {
                    Ok(()) => println!("  ↪ 已入队 steer（下一采样边界生效）"),
                    Err(error) => println!("  ✗ steer 被拒绝: {}", error.message),
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
    host.shutdown()?;
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
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }),
    );
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
    use std::sync::atomic::AtomicUsize;

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

    struct ImmediateText;
    impl model_provider::ChatModel for ImmediateText {
        fn stream_chat(
            &self,
            _: &protocol::ModelCallSpec,
            _: &[model_provider::ChatMessage],
            _: Option<&serde_json::Value>,
        ) -> Result<model_provider::ChatReply, ErrorEnvelope> {
            Ok(model_provider::ChatReply {
                text: "ok".into(),
                finish_reason: Some("stop".into()),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    /// TASK-803 验收 1：生产装配后模型可调用 fs_* 工具且路径绑定 --workspace。
    #[test]
    fn production_assembly_advertises_and_dispatches_fs_tools() {
        let workspace = tmp("ws-803");
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("note.txt"), "hello 803").unwrap();
        let mut registry = ToolRegistry::default();
        register_chat_tools(
            &mut registry,
            &workspace,
            None,
            &[],
            "http://127.0.0.1:1",
            &tools::CancellationToken::default(),
        )
        .unwrap();
        let names: Vec<&str> = registry.names().collect();
        for expected in [
            "fs_read",
            "fs_write",
            "fs_edit",
            "fs_glob",
            "fs_grep",
            "echo",
            "now",
            "web_fetch",
            "memory_write",
        ] {
            assert!(names.contains(&expected), "缺少工具 {expected}: {names:?}");
        }
        // 工作区外的路径不受信任（fs_read 越界拒绝）
        let outcome = registry
            .dispatch("fs_read", &serde_json::json!({ "path": "../outside.txt" }))
            .unwrap();
        assert!(matches!(outcome, ToolOutcome::Failure { .. }));
        // 工作区内可读
        let outcome = registry
            .dispatch("fs_read", &serde_json::json!({ "path": "note.txt" }))
            .unwrap();
        match outcome {
            ToolOutcome::Success { value } => assert_eq!(value["content"], "hello 803"),
            other => panic!("expected workspace read, got {other:?}"),
        }
        std::fs::remove_dir_all(&workspace).ok();
    }

    /// TASK-803 验收 2：显式 plugin_root 装配插件；不指定时不加载任何插件。
    #[test]
    fn plugin_root_is_opt_in_and_quarantined_plugins_do_not_shadow() {
        let workspace = tmp("ws-plugins");
        let plugin_root = tmp("plugins-803");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&plugin_root);
        std::fs::create_dir_all(&workspace).unwrap();
        let payload = r#"{"greeting":"from plugin"}"#;
        let dir = plugin_root.join("greeter");
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
        // 坏插件与好插件并存
        let bad = plugin_root.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("manifest.json"),
            r#"{"name":"bad","version":"1","payload":"payload.json","hash":"fnv1a:0000000000000000","tools":[]}"#,
        )
        .unwrap();
        std::fs::write(bad.join("payload.json"), r#"{"evil":true}"#).unwrap();

        let mut registry = ToolRegistry::default();
        register_chat_tools(
            &mut registry,
            &workspace,
            Some(&plugin_root),
            &[],
            "http://127.0.0.1:1",
            &tools::CancellationToken::default(),
        )
        .unwrap();
        let names: Vec<&str> = registry.names().collect();
        assert!(names.contains(&"greeter_hello"), "{names:?}");
        assert!(!names.contains(&"bad_any"), "隔离插件不得注册工具");
        let outcome = registry
            .dispatch("greeter_hello", &serde_json::json!({}))
            .unwrap();
        match outcome {
            ToolOutcome::Success { value } => assert_eq!(value["greeting"], "from plugin"),
            other => panic!("expected plugin payload, got {other:?}"),
        }

        // 不显式给 plugin_root：不自动信任工作区插件
        let mut ungated = ToolRegistry::default();
        register_chat_tools(
            &mut ungated,
            &workspace,
            None,
            &[],
            "http://127.0.0.1:1",
            &tools::CancellationToken::default(),
        )
        .unwrap();
        assert!(!ungated.names().any(|name| name == "greeter_hello"));
        std::fs::remove_dir_all(&workspace).ok();
        std::fs::remove_dir_all(&plugin_root).ok();
    }

    /// TASK-803 验收 3：/steer 的入队路径落 UserInputQueued 事件。
    #[test]
    fn steer_command_enqueues_user_input_event() {
        let path = tmp("steer-803.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let registry = ToolRegistry::default();
        let model = ImmediateText;
        let mut lp = AgentLoop::with_chat(
            &mut session,
            &registry,
            &model,
            protocol::ModelCallSpec {
                model: "m".into(),
                base_url: "http://localhost".into(),
                temperature: None,
            },
        );
        // 与 cmd_chat 的 /steer 分支相同的调用路径
        lp.enqueue_input("优先处理 X").unwrap();
        lp.mark_queued_inputs_consumed();
        let events = replay_session(&path).unwrap();
        assert!(events
            .iter()
            .any(|sequenced| matches!(&sequenced.event, Event::UserInputQueued { text } if text == "优先处理 X")));
        std::fs::remove_file(&path).ok();
    }

    /// TASK-803 验收 4：生产结果中间件——超预算结果被截断替换，小结果原样放行。
    #[test]
    fn production_middleware_redacts_oversized_results() {
        let middleware = ProductionResultMiddleware::with_max_result_bytes(32).unwrap();
        let outcome = ToolOutcome::Success {
            value: serde_json::json!({ "data": "x".repeat(128) }),
        };
        let context = ToolResultContext {
            call_id: "c1",
            tool: "fs_read",
            outcome: &outcome,
        };
        match middleware.inspect(&context).unwrap() {
            ToolResultDecision::Redact(ToolOutcome::Success { value }) => {
                assert_eq!(value["truncated_by_result_guard"], true);
                assert!(value["original_bytes"].as_u64().unwrap() > 32);
            }
            other => panic!("expected redaction, got {other:?}"),
        }
        let small = ToolOutcome::Success {
            value: serde_json::json!({ "data": "tiny" }),
        };
        let context = ToolResultContext {
            call_id: "c2",
            tool: "fs_read",
            outcome: &small,
        };
        assert!(matches!(
            middleware.inspect(&context).unwrap(),
            ToolResultDecision::Allow
        ));
    }

    /// TASK-705 验收：memory_write 落事件、注入幂等、resume 重建包含记忆系统消息。
    #[test]
    fn memory_write_event_injection_and_rebuild_roundtrip() {
        let path = tmp("memory.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut js = JsonlSession::create(path.clone()).unwrap();
        let mut registry = ToolRegistry::default();
        register_memory_tool(&mut registry);
        let result = registry
            .dispatch_with_audit(
                "memory_write",
                &serde_json::json!({ "text": "用户偏好 Rust", "tags": ["lang"] }),
            )
            .unwrap();
        assert!(matches!(result.outcome, ToolOutcome::Success { .. }));
        assert_eq!(result.audits.len(), 1);
        // agent-loop 的审计分支出事件——此处手动模拟同一行为以验证幂等注入
        js.append(Event::MemoryRecorded {
            memory_id: "mem-0".into(),
            text: "用户偏好 Rust".into(),
            tags: vec!["lang".into()],
            source: protocol::MemorySource::Model,
            scope: protocol::MemoryScope::LineageOnly,
        })
        .unwrap();
        // 首次注入：发生
        assert!(inject_memories(&mut js).unwrap());
        // 二次注入：幂等跳过
        assert!(!inject_memories(&mut js).unwrap());
        // 空白文本 fail-closed 且零审计事实
        let blank = registry
            .dispatch_with_audit("memory_write", &serde_json::json!({ "text": "  " }))
            .unwrap();
        match blank.outcome {
            ToolOutcome::Failure { error } => {
                assert_eq!(error.code, protocol::ErrorCode::ToolArgsInvalid);
            }
            other => panic!("expected blank rejection, got {other:?}"),
        }
        assert!(blank.audits.is_empty());
        // resume 重建：记忆以 SystemSummary 出现在历史头部
        let history = rebuild_history(&path).unwrap();
        assert!(history[0].content.contains("Known persistent memories"));
        assert!(history[0].content.contains("用户偏好 Rust"));
        std::fs::remove_file(&path).ok();
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
                timeout_ms: None,
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
                assert_eq!(error.code, protocol::ErrorCode::SandboxDenied);
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

    /// TASK-808 验收：scripted 端到端——「读仓库 → 定位 → 编辑 → 跑测试 → 完成」
    /// 全程走生产装配（register_chat_tools + 受限 exec + CAS），离线无 key。
    #[test]
    fn scripted_end_to_end_code_task_through_production_assembly() {
        use approval::{Decision, EscalationRequest};
        use sandbox_exec::{CommandSpec, ExecutionOutput, RestrictedBackend};

        struct AlwaysApprove;
        impl approval::Approver for AlwaysApprove {
            fn decide(&self, _: &EscalationRequest) -> Decision {
                Decision::Approved
            }
        }

        struct ScriptedBackend {
            workspace: PathBuf,
        }
        impl RestrictedBackend for ScriptedBackend {
            fn environment(&self) -> std::io::Result<protocol::ExecutorEnvironment> {
                Ok(protocol::ExecutorEnvironment {
                    os: "windows".into(),
                    home: std::env::temp_dir().display().to_string(),
                    workspace: self.workspace.display().to_string(),
                    generation: 0,
                })
            }

            fn execute(&self, _: &CommandSpec) -> std::io::Result<ExecutionOutput> {
                Ok(ExecutionOutput {
                    process_id: std::process::id() + 7,
                    exit_code: 0,
                    stdout: b"tests: 1 passed".to_vec(),
                    stderr: Vec::new(),
                    restricted: true,
                })
            }
        }

        let workspace = tmp("e2e-repo");
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn add(a: i32) -> i32 {\n    a + 1\n}\n",
        )
        .unwrap();

        // 生产装配（与 cmd_chat 同一入口）+ 受限 exec
        let cancel_token = tools::CancellationToken::default();
        let mut registry = ToolRegistry::default();
        register_chat_tools(
            &mut registry,
            &workspace,
            None,
            &[],
            "http://127.0.0.1:1",
            &cancel_token,
        )
        .unwrap();
        registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
        register_exec_tool(
            &mut registry,
            ScriptedBackend {
                workspace: workspace.clone(),
            },
            Some(std::sync::Arc::new(AlwaysApprove)),
        );

        // 脚本：grep 定位 → 读取（拿 hash）→ CAS 编辑 → 跑测试 → 完成
        let script = Mutex::new(vec![
            serde_json::json!({ "tool": "fs_grep", "args": { "query": "a + 1", "glob": "**/*.rs" } }),
            serde_json::json!({ "tool": "fs_read", "args": { "path": "src/lib.rs" } }),
            serde_json::json!("READ_THEN_EDIT"),
            serde_json::json!({ "tool": "exec", "args": { "program": "cargo", "args": ["test"], "sandbox_permissions": "workspace-write", "justification": "run the project tests" } }),
            // 上面第 3 步是哨兵：READ_THEN_EDIT 用前一步 fs_read 的 hash 构造 CAS 编辑
        ]);
        struct DispatchingModel<'a> {
            script: &'a Mutex<Vec<serde_json::Value>>,
            hash: std::sync::Mutex<Option<String>>,
            step: AtomicUsize,
        }
        impl model_provider::ChatModel for DispatchingModel<'_> {
            fn stream_chat(
                &self,
                _: &protocol::ModelCallSpec,
                _: &[model_provider::ChatMessage],
                _: Option<&serde_json::Value>,
            ) -> Result<model_provider::ChatReply, ErrorEnvelope> {
                let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let action = {
                    let mut queue = self.script.lock().unwrap();
                    if queue.is_empty() {
                        None
                    } else {
                        Some(queue.remove(0))
                    }
                };
                let Some(action) = action else {
                    return Ok(model_provider::ChatReply {
                        text: "修复完成，测试通过".into(),
                        finish_reason: Some("stop".into()),
                        tool_calls: vec![],
                        usage: None,
                    });
                };
                // 特殊步骤：读取上一步 fs_read 的 hash 再构造 CAS 编辑
                if action.as_str() == Some("READ_THEN_EDIT") {
                    let hash = self.hash.lock().unwrap().clone().expect("hash captured");
                    return Ok(model_provider::ChatReply {
                        text: String::new(),
                        finish_reason: Some("tool_calls".into()),
                        tool_calls: vec![model_provider::ToolCallRequest {
                            id: format!("call_edit_{step}"),
                            name: "fs_edit".into(),
                            arguments: serde_json::json!({
                                "path": "src/lib.rs",
                                "old_string": "a + 1",
                                "new_string": "a + 2",
                                "expected_hash": hash
                            })
                            .to_string(),
                        }],
                        usage: None,
                    });
                }
                let tool = action["tool"].as_str().unwrap().to_string();
                let arguments = action["args"].clone();
                Ok(model_provider::ChatReply {
                    text: String::new(),
                    finish_reason: Some("tool_calls".into()),
                    tool_calls: vec![model_provider::ToolCallRequest {
                        id: format!("call_{step}"),
                        name: tool,
                        arguments: serde_json::to_string(&arguments).unwrap(),
                    }],
                    usage: None,
                })
            }
        }

        // hash 捕获：借助一层 fs_read 结果缓存（真实装配中模型自行读取返回值）
        let hash_holder: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        // 先用 fs_read 拿到 hash（模拟模型读到返回值）
        let probe_registry_read = {
            let set = tools::FsToolSet::new(&workspace).unwrap();
            let mut reg = ToolRegistry::default();
            set.register(&mut reg);
            reg
        };
        let read_result = probe_registry_read
            .dispatch("fs_read", &serde_json::json!({ "path": "src/lib.rs" }))
            .unwrap();
        match read_result {
            ToolOutcome::Success { value } => {
                *hash_holder.lock().unwrap() = Some(value["hash"].as_str().unwrap().to_string());
            }
            other => panic!("probe read failed: {other:?}"),
        }

        let path = tmp("e2e.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let model = DispatchingModel {
            script: &script,
            hash: std::sync::Mutex::new((*hash_holder.lock().unwrap()).clone()),
            step: AtomicUsize::new(0),
        };
        let mut lp = AgentLoop::with_chat(
            &mut session,
            &registry,
            &model,
            protocol::ModelCallSpec {
                model: "scripted".into(),
                base_url: "http://localhost".into(),
                temperature: None,
            },
        );
        let middleware = ProductionResultMiddleware::with_max_result_bytes(256 * 1024).unwrap();
        lp.result_middleware = Some(&middleware);
        lp.loop_guard = Some(LoopGuard {
            remind_after: 3,
            reject_after: 8,
        });
        lp.mark_queued_inputs_consumed();
        lp.inbox.push("修复 add 函数");
        assert_eq!(lp.run_turn(), 1);

        // 断言：文件真实被 CAS 编辑
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/lib.rs")).unwrap(),
            "pub fn add(a: i32) -> i32 {\n    a + 2\n}\n"
        );
        // 断言：事件轨迹完整（grep → read → edit → exec → 完成），exec 有审批审计
        let events = replay_session(&path).unwrap();
        let tools_called: Vec<String> = events
            .iter()
            .filter_map(|sequenced| match &sequenced.event {
                Event::ToolCallRequested { tool, .. } => Some(tool.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools_called,
            vec![
                "fs_grep".to_string(),
                "fs_read".to_string(),
                "fs_edit".to_string(),
                "exec".to_string()
            ]
        );
        assert!(events.iter().any(|sequenced| matches!(
            &sequenced.event,
            Event::ApprovalDecided { approved: true, .. }
        )));
        assert!(matches!(
            events.last().unwrap().event,
            Event::TurnCompleted { .. }
        ));
        std::fs::remove_dir_all(&workspace).ok();
        std::fs::remove_file(&path).ok();
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
        let read_back = registry
            .dispatch("fs_read", &serde_json::json!({ "path": "note.txt" }))
            .expect("read back must succeed");
        let ToolOutcome::Success { value: read_value } = read_back else {
            panic!("read must succeed");
        };
        let hash = read_value["hash"].as_str().unwrap().to_string();
        let edit = registry
            .dispatch(
                "fs_edit",
                &serde_json::json!({
                    "path": "note.txt",
                    "old_string": "one",
                    "new_string": "two",
                    "expected_hash": hash
                }),
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
