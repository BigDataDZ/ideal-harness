# ideal-harness

一个 **protocol-first、事件溯源、三层沙箱、fail-closed 审批** 的 LLM Agent Harness 原型。

> 设计依据来自对 OpenAI Codex CLI 与 DeepSeek Harness 两个成熟实现的源码级对比研究——
> 取其共识（事件溯源、ErrorCode 路由、OS 级沙箱），避其教训（巨石 core / 过度碎片化）。
> 每个决策的对标记录见 [docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md)。

## 当前状态：可对话 MVP 已达成（v0.2）✅

✅ 已实现：协议层（含流式事件契约）/ 三档沙箱抽象 / fail-closed 审批 / 工具注册与调度 /
JSONL 事件溯源（append/replay/fork）/ 状态机主循环 / 双触发压缩判定 /
OpenAI 兼容流式模型客户端（故障注入测试）/ 工具调用闭环 / `ideal-harness chat` 多轮对话
（会话持久化 + 崩溃恢复）——真实 API key 端到端冒烟通过（[记录](tests/manual/chat-smoke.md)）

⏳ 进行中（P2 安全纵深）：受限执行进程池、网络白名单代理、人工审批通道

测试基线：55 passed · CI：GitHub Actions（fmt / clippy -D warnings / test，Ubuntu + Windows 双平台）

## 快速开始

```bash
# 构建 + 测试（约 30 秒；依赖已缓存时可 --offline）
cargo build --workspace
cargo test --workspace

# 运行最小演示：沙箱拦截 / 工具参数自纠 / 事件流回放
cargo run -p harness-cli

# 或直接使用编译产物（Windows）
target\release\ideal-harness.exe
```

要求：Rust 1.85+（edition 2021）。

## 架构一图流

```
客户端面（TUI/Web/IDE，纯投影） ──RPC+SSE──▶ Host 进程
   ├ protocol     唯一契约（Event/ErrorCode）
   ├ agent-loop   Phase 状态机 + Inbox 唤醒
   ├ session      JSONL 事件溯源（append/replay/fork）
   ├ tools        注册表 + schema 校验 + 屏障调度
   ├ context      token 预算 + 双触发压缩
   ├ model-provider  OpenAI 兼容 HTTP+SSE 客户端
   ├ approval     fail-closed 提权审批
   └ sandbox-policy  SandboxMode 单一抽象贯穿三层
                                    ▼
                    受限执行进程池 + 网络白名单代理(P2)
```

## 文档地图

| 文档 | 内容 |
|---|---|
| [AGENTS.md](AGENTS.md) | 协同开发入口：模块所有权、红线、工作循环 |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | 完整开发规范 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | P0~P5 演进路线与任务卡 |
| [docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md) | D1~D13 决策对标（学谁/不同于谁/坑在哪） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献指南 |
| [SECURITY.md](SECURITY.md) | 安全漏洞报告 |

## 设计原则速览

1. Harness 的本质：把不可靠的模型输出转化为可靠的系统行为
2. 错误按稳定 `ErrorCode` 路由，永不解析 message 字符串
3. fail-closed 是底线：审批服务不在场 = 拒绝
4. 一切自动行为必须落 Event（可解释性优先于流畅感）
5. 裁剪上下文永不拆散 tool_call/result 配对

## License

Apache-2.0 —— 见 [LICENSE](LICENSE)。
