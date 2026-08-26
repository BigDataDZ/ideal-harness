# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式，
版本号遵循语义化版本（SemVer）。

## [Unreleased]

### Added

- **agent-loop（TASK-103）**：工具调用闭环——采样返回 tool_call → registry.dispatch → ToolResultAdded 回填 → 继续采样直至文本答复；`max_tool_rounds` 超限强制终结并落 `TurnAborted`；tool_call/result 严格配对由测试锁定；未知工具与非法 JSON 参数均回 `ToolArgsInvalid` 自纠码且不触发 handler
- **model-provider（TASK-103 配套）**：SSE 流式 `tool_calls` 分片按 index 聚合（id/name 取首见、arguments 拼接）；`ChatMessage` 支持 assistant 工具调用与 tool 结果回填消息；`ChatModel::stream_chat` 增加 tools 广告参数
- **protocol（TASK-101）**：`Event::ModelChunkReceived` 流式增量事件、`ModelCallSpec` 调用规格（无认证字段，属 provider 层）；旧版 JSONL 向后兼容由测试锁定
- **model-provider（TASK-102）**：OpenAI 兼容阻塞式 `chat/completions` SSE 客户端（reqwest + rustls，离线缓存可解析）；纯解析层 `parse_sse_line` / `extract_delta` 可独立测试；错误仅按结构化字段映射稳定码（超限→ContextWindowExceeded、超时/断流/截断→ModelStreamBroken）；API key 读 `IDEAL_HARNESS_API_KEY`，缺失即 fail-closed 拒绝；本地 TcpListener 故障注入测试覆盖超时/截断/非 JSON 行/半途断连

### 计划中（详见 docs/ROADMAP.md）

- P1 可对话 MVP：真实 LLM provider 接入（TASK-102~104）、工具调用闭环
- P2 安全纵深：受限执行进程池、网络白名单代理、人工审批通道

## [0.1.0] - 2026-08-22

### Added（架构验证原型）

- `protocol` crate：Event 事件流 / ErrorCode 稳定错误码 / ErrorEnvelope，wire 契约 + serde 往返测试
- `sandbox-policy` crate：SandboxMode 三档单一抽象、词法栅栏、提权加宽表
- `approval` crate：fail-closed 审批流、提权参数成对校验（8 个测试覆盖全部分支）
- `tools` crate：工具注册表、JSON Schema 骨架校验、屏障式调度（缺参不触发 handler）
- `session` crate：JSONL 事件溯源 append/replay/fork，崩溃恢复与坏行报错测试
- `context` crate：token 压力阈值、溢出码映射、tool 配对完整性判定
- `agent-loop` crate：Phase 状态机、Inbox 唤醒、单活跃 turn 契约、故障注入测试
- `harness-cli`：装配演示入口（沙箱拦截/参数自纠/事件回放）
- 协同开发规范体系：AGENTS.md / docs/DEVELOPMENT.md / docs/ROADMAP.md / docs/DESIGN-DECISIONS.md

### Fixed

- sandbox-policy：ReadOnly 模式误入 WorkspaceWrite 分支导致只读态可写（由单元测试捕获后修复）
