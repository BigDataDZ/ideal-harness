# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式，
版本号遵循语义化版本（SemVer）。

## [Unreleased]

### Added

- **tools/harness-cli（TASK-701）**：内置文件工具集 `fs_read/fs_write/fs_edit/fs_glob/fs_grep`——以 canonical 工作区根为信任边界（symlink 拒绝 + 词法穿越栅栏），覆盖写与编辑强制 read-before-write，超大文件与超限结果集全文落 `.harness/spill`、结果仅带预览 + 可被 `fs_read` 取回的 locator
- **tools/model-provider/harness-cli（TASK-703）**：`web_fetch` 工具——仅 http/https、私网/回环主机一律拒绝（SSRF）、主机白名单默认拒绝、重定向逐跳复检、内容超限 spill；物理出网由 `Fetcher` 抽象承载，生产实现经本地 CONNECT 白名单代理（`http_fetch_via_proxy`：禁自动重定向、硬字节上限、仅回环代理）
- **protocol/tools/agent-loop（TASK-702）**：新增 `ToolTimeout`/`ToolLoopDetected` 稳定错误码；`ToolSpec.timeout_ms` 为单次工具执行设 deadline（独立线程限时等待，超时不取消底层副作用）；agent-loop 可选 `LoopGuard` 对同一工具连续等参调用先在结果中附提醒、达到上限后不再触发 handler 并以稳定码拒绝，未配置时行为与既有完全一致
- **protocol/agent-loop/session（TASK-704）**：`UserInputQueued` 事件承载 turn 运行中入队的 steer 输入；模型表面投影将其视同 User 消息并在工具批次未闭合时延迟出账以保住配对；agent-loop 以持久游标在每个采样轮边界吸收排队输入，跨 turn 残留由下一 turn 接管，resume 与在线视图一致
- **protocol/session/tools/agent-loop/harness-cli（TASK-705）**：`MemoryRecorded` 事件存储跨会话记忆（同 id 后写覆盖），`MemoryContextInjected` 事件把记忆摘要以加法式系统消息送入模型表面；`memory_write` 工具经 `ToolAudit::MemoryRecorded` 由 agent-loop 落事件；CLI 注入幂等，resume 重建与在线视图一致
- **sandbox-exec（TASK-706，libc 仅 Linux target）**：Linux Landlock 生产后端——子进程 `PR_SET_NO_NEW_PRIVS` 后按 ABI v1 声明全盘只读+执行、WorkspaceWrite 档额外授予工作区根全量文件权，未声明访问默认拒绝，无 Landlock 内核 fail-closed 不降级；`PlatformRestrictedBackend` 在 Linux 组合该后端
- **tools/agent-loop/harness-cli（TASK-607，D17）**：新增可信插件目录（`.harness/plugins/*/manifest.json`）与工具结果安全中间件；manifest 声明 payload 哈希与工具能力，未知字段拒绝，路径逃逸/哈希漂移/未声明能力在注册与调度两个时点 fail-closed，坏插件被隔离而不遮蔽好插件；工具结果进模型表面前可检查、脱敏或拒绝，插件来源结果在中间件缺席或失败时 fail-closed 并经 `ToolResultAdded` 留痕
- **protocol/session/agent-loop/harness-cli（TASK-601）**：新增唯一 Model Surface 契约与事件投影；模型工具调用批次和压缩 replace-prefix/source seq 可忠实重放，Hook 等纯审计调用不会混入 resume 上下文；旧 JSONL 保持可读
- **protocol/context/model-provider/agent-loop（TASK-602）**：新增可重放根 Token 预算与 usage 事件账本；provider usage 优先、启发式兜底，主代理和两层 subagent 用量按 agent path 汇总 own/subtree，耗尽后在下一次 provider/runner 调用前拒绝
- **protocol/sandbox-policy/sandbox-exec/approval/tools（TASK-603，D16）**：审批绑定权限配置 epoch、稳定摘要与 executor OS/home/workspace/generation；审批期间策略、工作区或执行目标变化会拒绝旧授权并记录失效事件，未知环境 fail-closed
- **protocol/session/agent-loop（TASK-606，D19）**：新增事件溯源 Agent Team roster、恰好一次 mailbox、revision/CAS 任务 DAG 与写范围冲突审计；崩溃后可由会话事件完整恢复，旧 revision、依赖环和成员权限扩大均 fail-closed
- **protocol/harness-cli（TASK-605，D18）**：只读 RPC 增加服务能力协商与 connection generation；SSE 支持标准 `Last-Event-ID`、follow-before-page 无窗口补洞和 generation 校验，旧连接与业务错误均 fail-closed 且标记为不可自动重试
- **tools/agent-loop（TASK-604，D17）**：新增 required/optional MCP 服务状态机、发现宽限、connection generation 与受监管工具句柄；可选服务失败独立降级、必需服务失败拒绝启动，旧代际调用 fail-closed，结果经过按工具裁剪、硬大小上限与来源一致性检查
- **protocol（TASK-504）**：新增只读会话 timeline、SSE event frame、查询参数与统一 RPC 错误 DTO，并增加 `SessionNotFound` / `CursorInvalid` 稳定错误码
- **harness-cli（TASK-504）**：新增 loopback-only `serve` 子命令；按请求重放 JSONL 真相源，支持 timeline 分页与 `last_seq` SSE 无重无漏补洞；非本机监听、写方法、路径穿越、坏 cursor 和未知会话均 fail-closed
- **protocol（TASK-203）**：新增 `NetworkAccessDenied` 稳定审计事件，记录被白名单代理拒绝的 host、port 与结构化原因
- **network-proxy（TASK-203）**：默认断网的 HTTP CONNECT 白名单代理；provider 主机精确放行，拒绝与审计服务失败均 fail-closed

## [0.2.0] - 2026-08-25

P1「可对话 MVP」达成：真实 API key 端到端冒烟通过
（工具调用闭环 + 跨轮记忆，记录见 `tests/manual/chat-smoke.md`）。

### Added

- **harness-cli（TASK-104）**：`ideal-harness chat` 交互子命令——stdin 多轮对话、`--session/--base-url/--model` 参数、会话复用（seq 续接 + 事件流重建历史）、悬空 turn 崩溃恢复（补记 TurnAborted）、无 key fail-closed 拒启动；冒烟记录见 `tests/manual/chat-smoke.md`
- **agent-loop（TASK-104 配套）**：`chat_history` 跨轮对话记忆（含最终答复回填历史的修复，由多轮记忆测试锁定）
- **agent-loop（TASK-103）**：工具调用闭环——采样返回 tool_call → registry.dispatch → ToolResultAdded 回填 → 继续采样直至文本答复；`max_tool_rounds` 超限强制终结并落 `TurnAborted`；tool_call/result 严格配对由测试锁定；未知工具与非法 JSON 参数均回 `ToolArgsInvalid` 自纠码且不触发 handler

### Fixed

- **model-provider**：assistant 工具调用消息的序列化形状由扁平改为 OpenAI 嵌套 `{"id","type":"function","function":{...}}`——此前工具结果回填后的二次采样会被上游 400 拒绝（真实冒烟发现，测试锁定）
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
