# ideal-harness

一个 **protocol-first、事件溯源、三层沙箱、fail-closed 审批** 的 LLM Agent Harness 原型。

> 设计依据来自对 OpenAI Codex CLI 与 DeepSeek Harness 两个成熟实现的源码级对比研究——
> 取其共识（事件溯源、ErrorCode 路由、OS 级沙箱），避其教训（巨石 core / 过度碎片化）。
> 每个决策的对标记录见 [docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md)。

## 当前状态：路线图 P8 已完成 ✅（真实模型冒烟待 key 执行，见 tests/manual/p8-smoke.md）

✅ 已实现：流式模型与工具闭环、三层沙箱与 fail-closed 审批、JSONL/zstd/SQLite 会话恢复、
上下文压缩与 spill、subagent 生命周期与角色策略、stdio MCP、可信 Skill、可审计 Hook，
loopback-only 的只读 timeline RPC 与按事件序号补洞的 SSE 投影，
运行时闭环（模型表面忠实重放、层级 Token 预算、权限 epoch/执行环境绑定、
受监管 MCP registry、generation-aware RPC、事件溯源 Agent Team、可信插件清单与结果中间件），
以及 P7 工具面（fs_read/write/edit/glob/grep 内置文件工具、白名单代理内的 web_fetch、
工具超时与循环防护、turn 内 steer 排队输入、跨会话记忆投影、Linux Landlock 生产后端）。

## 快速开始

```bash
# 构建 + 测试（约 30 秒；依赖已缓存时可 --offline）
cargo build --workspace
cargo test --workspace

# 运行最小演示：沙箱拦截 / 工具参数自纠 / 事件流回放
cargo run -p harness-cli

# 或直接使用编译产物（Windows）
target\release\ideal-harness.exe

# 启动只读会话投影；目录内会话名采用 <session-id>.jsonl
cargo run -p harness-cli -- serve --root .\sessions --bind 127.0.0.1:8765
```

只读接口：

- `GET /v1/sessions/<id>/timeline?cursor=0&limit=20`
- `GET /v1/sessions/<id>/events?last_seq=41`（SSE，仅返回 `seq > 41`）

服务拒绝非 loopback 监听、写方法、非法会话 ID、未知会话和越界 cursor；每次查询均从会话事件日志重放，不维护第二真相源。

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
