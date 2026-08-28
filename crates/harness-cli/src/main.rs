//! 装配入口：`ideal-harness`（最小演示）/ `ideal-harness chat`（可对话 MVP，TASK-104）。
//! chat 模式 = 真实模型 + 工具调用闭环 + JSONL 会话持久化 + 中断恢复。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_loop::{AgentLoop, ModelProvider};
use approval::TerminalApprover;
use model_provider::{ChatMessage, OpenAiCompatClient};
use protocol::{ErrorEnvelope, Event, ModelCallSpec, ToolOutcome};
use sandbox_exec::PlatformRestrictedBackend;
use sandbox_policy::{SandboxMode, SandboxPolicy};
use session::{replay as session_replay, JsonlSession};
use tools::{EscalationAvailability, ToolRegistry, ToolSpec};

mod security;
use security::{register_exec_tool, ProviderProxy};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("chat") => cmd_chat(&args[1..]),
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
}

impl Default for ChatArgs {
    fn default() -> Self {
        Self {
            session: std::env::temp_dir().join("ideal-harness-chat.jsonl"),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
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
        openai_tools_json(&registry, &["echo", "now", "exec"])
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
                for name in ["echo", "now", "exec"] {
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
        for se in session_replay(lp.session.path())?
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
fn recover_dangling_turn(session: &mut JsonlSession) -> anyhow::Result<bool> {
    let events = session_replay(session.path())?;
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

/// 从事件流重建模型可见历史：user/assistant 成对回放；
/// 工具调用中间态由该 turn 的最终 assistant 文本概括（MVP 语义，P3 压缩接管）。
fn rebuild_history(path: &Path) -> anyhow::Result<Vec<ChatMessage>> {
    let mut out = Vec::new();
    for se in session_replay(path)? {
        match se.event {
            Event::UserMessage { text } => out.push(ChatMessage::user(text)),
            Event::AssistantMessage { text } if !text.is_empty() => {
                out.push(ChatMessage::assistant(text))
            }
            _ => {}
        }
    }
    Ok(out)
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
    for se in session_replay(&path)? {
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
        let evs = session_replay(&path).unwrap();
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
    fn rebuild_history_pairs_user_and_assistant_only() {
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
    fn openai_tools_json_wraps_registered_specs() {
        let mut reg = ToolRegistry::default();
        register_demo_tools(&mut reg);
        let v = openai_tools_json(&reg, &["echo", "now"]).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "echo");
        assert_eq!(v[1]["function"]["name"], "now");
    }
}
