//! 装配入口：把全部模块接起来，跑一个最小可验证的演示 turn。
//! 演示三件事：① 沙箱模式语义 ② 工具校验拦截缺参并回传自纠码 ③ 事件溯源回放。

use agent_loop::{AgentLoop, ModelProvider};
use protocol::ToolOutcome;
use sandbox_policy::{SandboxMode, SandboxPolicy};
use session::JsonlSession;
use tools::{ToolRegistry, ToolSpec};

/// 演示用模型提供者：恒定回声。
struct EchoProvider;

impl ModelProvider for EchoProvider {
    fn complete(&self, _user_text: &str) -> Result<String, protocol::ErrorEnvelope> {
        Ok("echo: 收到".into())
    }
}

fn main() -> anyhow::Result<()> {
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
    registry.register(
        ToolSpec {
            name: "echo".into(),
            description: "回声工具（演示用）".into(),
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
    for se in session::replay(&path)? {
        println!("{:>3}  {}", se.seq, serde_json::to_string(&se.event)?);
    }
    Ok(())
}
