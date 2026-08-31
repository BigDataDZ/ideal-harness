# ideal-harness 开发演进路线图（ROADMAP）

> 版本 v0.1 | 配套规范：`AGENTS.md`（操作入口）、`DEVELOPMENT.md`（完整规范）
> 本文件是任务卡的唯一来源；领取前确认无重叠认领。

---

## 一、阶段总览

| 阶段 | 版本 | 主题 | 出口判据 |
|---|---|---|---|
| P0 | v0.1 ✅ | 架构骨架 + 协同规范 | 8 crate / 29 测试全绿（已完成） |
| P1 | v0.2 ✅ | **可对话 MVP**：接真实 LLM + 工具调用闭环 | 出口判据已达成：真实 API key 端到端冒烟通过（`tests/manual/chat-smoke.md`） |
| P2 | v0.3 ✅ | **安全纵深**：OS 沙箱 + 网络代理 + 人工审批 | 出口判据已由 TASK-206 端到端测试证明，并经人类守护者确认 |
| P3 | v0.4 ✅ | **上下文工程**：压缩/spill/token 计量 | 长会话不触顶；溢出自动恢复有测试 |
| P4 | v0.5 ✅ | **会话产品化**：resume/fork/投影存储 | 断点续聊；fork 分支独立演化 |
| P4.1 | v0.5.1 ✅ | **可靠性收口**：统一存储、崩溃恢复、subagent 治理 | 压缩会话可恢复；子代理资源与权限不越界 |
| P5 | v0.6 ✅ | **扩展生态**：MCP/skill/hooks/Web 投影 | 第三方工具可控接入；客户端断线可按 seq 补洞 |
| P6 | v0.7 ✅ | **运行时闭环**：忠实重放、层级预算、权限时效与受监管扩展 | resume 与在线上下文等价；预算/权限/连接状态不可被旧状态绕过 |
| P7 | v0.8 ✅ | **工具面扩展**：内置文件工具、执行护栏、白名单 web_fetch、turn 内 steer、跨会话记忆、Landlock 后端 | 出口判据由 TASK-803/808 收口：生产 CLI 装配 + scripted 端到端已证 |
| P8 | v0.9 🟡 | **安全与产品化收口**：出网目标钉扎、可取消工具、生产装配、并发一致性、跨平台证据 | 代码与自动化门禁已完成；Windows/Linux CI 与安全回归全绿；发布验收仅待真实模型冒烟 |
| P9 | v1.0 ⬜ | **桌面客户端**：Tauri 2 + React/TypeScript，事件流纯投影，安全审批与跨平台安装包 | Windows 桌面端可完成会话、流式对话、工具审计和审批；不产生第二真相源；安装包和 UI E2E 门禁通过 |

并行规则：**同一阶段内不同 crate 的任务卡可由多个智能体并行认领；涉及 protocol 的任务串行**（契约冻结原则）。

---

## 二、P1 可对话 MVP（v0.2）

### TASK-101: 协议扩展——模型流式契约 ✅（commit `b661d4f`）
- 目标 crate: protocol ⚠️串行
- 内容：Event 增加 `ModelChunkReceived`；新增 `ModelCallSpec`（model/base_url/temperature）；ErrorCode 已含 ModelStreamBroken 无需改
- 验收：序列化往返测试；旧 JSONL 文件仍可重放（向后兼容证明）
- 明确不做：不引入认证字段（属 provider 层）

### TASK-102: 新 crate model-provider——OpenAI 兼容客户端 ✅（commit `c880d0f`）
- 目标 crate: model-provider（新建，允许依赖 protocol；加入所有权地图）
- 内容：HTTP POST + SSE 流式解析；错误映射：HTTP 4xx→ToolArgsInvalid 类比归 Internal，超限→ContextWindowExceeded，断流→ModelStreamBroken；API key 读环境变量 `IDEAL_HARNESS_API_KEY`
- 验收：mock HTTP server 三种故障注入测试（超时/截断/非 JSON 行）
- 明确不做：不做多 provider 抽象层（一个够用，接口留 trait）

### TASK-103: agent-loop 工具调用闭环 ✅（commit `c312abd`）
- 目标 crate: agent-loop（依赖 model-provider 加入地图）
- 内容：采样返回 tool_call → registry.dispatch → ToolResultAdded 追加 → 继续采样直至模型给出文本答复（最大轮次保护，防死循环）
- 验收：mock provider 测试「文本→工具→文本」三段序列；超过 max_tool_rounds 强制终结并留 Event
- 依赖：TASK-101

### TASK-104: cli chat 子命令 ✅（commit `e454107`，冒烟记录 tests/manual/chat-smoke.md）
- 目标 crate: harness-cli
- 内容：`ideal-harness chat` 进入交互循环（stdin 读行）；`--session <path>` 复用既有 JSONL；Ctrl+C 优雅中止当前 turn 留 TurnAborted
- 验收：手动冒烟脚本记录到 tests/manual/chat-smoke.md
- 依赖：TASK-103

---

## 三、P2 安全纵深（v0.3）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-201 ✅（commit `826b2c2`） | tools | 完整 JSON Schema 校验器（type/enum/items/additionalProperties） | 对 DSH 风格 schema 的兼容测试集 |
| TASK-202 ✅（commit `beacba2`） | sandbox-exec(新) | 受限执行进程池：Windows CreateRestrictedToken 优先，预留 Landlock trait 位 | 外部命令不在主进程内执行的架构测试 |
| TASK-203 ✅（commit `8d74044`） | network-proxy(新) | 白名单代理进程；provider 域名默认放行，其余默认拒绝 | 断网默认态测试；白名单外连接被拒且落审计事件 |
| TASK-204 ✅（commit `ad502ce`） | approval | CLI 人工审批通道（终端 y/n）+ Approver trait 实现 | fail-closed 全分支已有测试基础上补集成测试 |
| TASK-205 ✅（commit `f040860`） | tools | 提权出口动态广告：仅受限后端挂载时注入 escalation 字段 | 只读模式下 schema 含提权字段、全开放模式不含 |
| TASK-206 ✅（commit `dd09f37`） | harness-cli+model-provider+tools+agent-loop | P2 安全链路端到端装配：命令仅走受限进程池，模型仅走白名单代理，提权决策写入事件流（D5/D7/D8/D9） | 外部 provider 无代理不可直连；命令子进程受限；无审批器提权失败；审批/拒网/结果均留痕 |

### TASK-206: P2 安全链路端到端装配 ✅（commit `dd09f37`）
- 目标 crate: harness-cli、model-provider、tools、agent-loop（复用 approval、sandbox-exec、network-proxy）
- 验收标准:
  1. chat 的模型请求只能通过本地 CONNECT 白名单代理；生产构造器不存在外部域名直连路径，空白/非本地代理 fail-closed
  2. 新增 exec 工具，所有外部命令仅通过 RestrictedProcessPool 执行，成功结果证明子进程与主进程隔离且 restricted=true
  3. 受限模式动态广告成对提权字段；无审批器或拒绝时返回稳定 ApprovalRejected，审批决定与工具结果按序写入 Event
  4. 白名单外网络请求被拒并产生 NetworkAccessDenied；全 workspace build/test/clippy/fmt 通过
- 明确不做: Linux Landlock 生产后端、Windows WFP、Web/TUI 审批、P3 压缩/spill、放宽协议契约
- 依赖: TASK-202、TASK-203、TASK-204、TASK-205

## 四、P3 上下文工程（v0.4）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-301 ✅（commit `ccbc6d8`） | context | TokenMeter：以 provider usage 为锚，启发式兜底 | usage 缺失时降级路径测试 |
| TASK-302 ✅（commit `08c1526`） | context+agent-loop | 两段式压缩：ToolResultPruner → LLM 摘要替换区间 | **配对完整性属性测试**（随机裁剪不拆对） |
| TASK-303 ✅（commit `0321297`） | agent-loop | 溢出强制压缩后自动重试（填现有 TODO 挂点） | 注入 ContextWindowExceeded 后观察压缩事件+成功重试 |
| TASK-304 ✅（commit `6b561c8`） | session | spill：超长工具结果全文落盘，事件中只存预览+locator | 取回句柄 roundtrip 测试 |

## 五、P4 会话产品化（v0.5）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-401 ✅（commit `e3e507b`） | harness-cli | `resume` / `fork` 子命令 | fork 后两会话独立追加互不影响 |
| TASK-402 ✅（commit `d0f22f4`） | session | SQLite 投影 + write-behind 编排 | 投影查询与 JSONL 重放一致性测试 |
| TASK-403 ✅（commit `a7b79d2`） | session | zstd 帧压缩（可选依赖，任务卡批准后引入） | 新旧格式互读迁移测试 |
| TASK-404 ✅（commit `4395c97`） | agent-loop(子模块) | 进程内 subagent 骨架 + report 回传事件 | 子代理失败不污染父会话 |

## 六、P4.1 可靠性收口（v0.5.1）

### TASK-405: 统一 SessionStore 抽象 ✅（commit `dbd8ca8`）
- 目标 crate: session、agent-loop、harness-cli
- 内容: 以对象安全接口统一 append/len/path；JsonlSession、zstd 会话与投影编排实现同一抽象；主循环和 CLI 不再硬绑定 JsonlSession
- 验收标准: 同一 AgentLoop 测试分别在 JSONL 与内存测试存储运行；CLI 恢复路径经统一 replay 入口；默认 feature 与 zstd feature 均编译
- 明确不做: 不改 protocol；不引入异步运行时；不改变现有 JSONL 线上格式

### TASK-406: 可恢复的 zstd 帧写入 ✅（commit `6317096`）
- 目标 crate: session
- 内容: 增加存储 header/格式版本、带校验帧、批量 append、sync durability、失败回滚及撕裂尾部恢复
- 验收标准: 注入半帧/坏 checksum/同步失败；只修复未提交尾部，提交前缀损坏必须拒绝；旧 TASK-403 文件仍可迁移读取
- 明确不做: 不过滤或重编号 Event；不把 SQLite 变成真相源；仅使用任务卡已批准的 zstd 依赖

### TASK-407: SQLite 增量投影与水位 ✅（commit `176f2f0`）
- 目标 crate: session
- 内容: 投影记录 source watermark 与 schema version；打开时只补齐缺失后缀，检测缺口/冲突后由 JSONL 原子重建
- 验收标准: 长日志 reopen 不全表重写；模拟 JSONL 领先、SQLite 领先和中间缺口；查询始终与 replay 一致
- 明确不做: 不在 SQLite 接受独立业务写入；不改变 Event 契约

### TASK-408: 会话 timeline 与非破坏性 revert ✅（commit `66ef6e5`）
- 目标 crate: session、harness-cli
- 内容: 从事件流派生 turn 边界索引；提供分页 timeline；revert 默认 fork 到指定 turn 之前而不改源会话
- 验收标准: 多 turn 分页无重无漏；revert 后源/目标独立追加；非法 turn/cursor fail-closed
- 明确不做: 不回滚工作区文件；不原地截断源日志；不新增 UI

### TASK-409: subagent 资源与选择策略 ✅（commit `bd83a8f`）
- 目标 crate: agent-loop
- 内容: 最大深度/并发、turn/token 预算、允许模型与工具 allow/deny；所有限制在运行器调用前检查
- 验收标准: 每种超限均返回稳定 ErrorCode 并形成完整父事件对；拒绝时 runner 零调用；子策略不得扩大父策略
- 明确不做: 不并行执行；不新增模型 provider；不改沙箱抽象

### TASK-410: subagent 生命周期、lineage 与 report 投递 ✅（commit `f571d09`）
- 目标 crate: protocol ⚠️串行、agent-loop、session
- 内容: 增加向后兼容的子代理生命周期/报告事件；父取消传播；记录 parent/child lineage；支持 next-step 与 quiet 报告投递
- 验收标准: 成功/失败/取消均有闭合事件序列；quiet 不唤醒父 inbox；next-step 仅在边界唤醒；旧 JSONL 可重放
- 明确不做: 不做跨进程 agent；不允许子代理继承外的权限；不删除现有事件变体

### TASK-411: 严格 Agent Role 配置 ✅（commit `680a034`）
- 目标 crate: agent-loop
- 内容: 定义角色描述、instructions、模型与工具约束；未知字段/空字段/重复昵称拒绝；角色覆盖不得扩大父策略
- 验收标准: 内置与用户角色解析测试；恶意/未知配置 fail-closed；角色能生成确定性的子任务配置
- 明确不做: 不引入 TOML/YAML 外部依赖；不实现远端角色市场

### TASK-412: 场景级事件轨迹快照 ✅（commit `8f04875`）
- 目标 crate: session、agent-loop、harness-cli（tests only）
- 内容: 建立无第三方快照依赖的 canonical JSONL 场景夹具，覆盖 resume/fork/zstd/SQLite/subagent 组合
- 验收标准: 至少 6 条端到端轨迹；差异输出可读；Windows 路径与并发端口不影响结果
- 明确不做: 不替代单元测试；不录制真实 API key/网络响应；不自动更新期望值

## 七、P5 扩展生态（v0.6）

### TASK-501: stdio MCP client 最小闭环 ✅（commit `f28bac7`）
- 目标 crate: tools、agent-loop
- 内容: stdio JSON-RPC 初始化/工具发现/调用；工具来源与每工具输出上限；结果复用现有 spill 与审批审计
- 验收标准: fixture server 完成发现与调用；超限输出被裁剪并可取回；协议错误/子进程退出 fail-closed 且不留未配对调用
- 明确不做: 不做 HTTP MCP/OAuth；不引入异步运行时；不信任服务端 message 做控制流

### TASK-502: 可信 Skill 目录发现 ✅（commit `70e439b`）
- 目标 crate: tools
- 内容: 发现 `.harness/skills/*/SKILL.md`；解析受限 YAML frontmatter；可信根 canonical 校验、指纹刷新与确定性目录
- 验收标准: 新增/修改/删除可刷新；遍历/软链接逃逸/重复名称拒绝；子代理只能继承父级已验证 skill
- 明确不做: 不引入 YAML 依赖；不执行 skill 内任意代码；不做远端下载

### TASK-503: Hook 生命周期最小集 ✅（commit `6904645`）
- 目标 crate: agent-loop
- 内容: pre/post_tool_use、turn_completed/failed/interrupted、subagent_stopped；Hook 结果留 Event，安全 Hook 缺席或失败时 fail-closed
- 验收标准: 正常/工具失败/turn 中断/子代理取消均按序触发；Hook 不得递归触发自身；失败不会拆散工具配对
- 明确不做: 不执行 shell hook；不做第三方插件加载；不允许 Hook 直接写 session

### TASK-504: 只读 RPC+SSE 会话投影 ✅（commit `9a3df13`）
- 目标 crate: protocol ⚠️串行、harness-cli
- 内容: loopback-only 只读服务，提供 session timeline 查询与按 seq SSE 补洞；所有线上 DTO 定义在 protocol
- 验收标准: 非 loopback 绑定拒绝；断线重连从 last_seq 无重无漏；坏 cursor/未知 session fail-closed；客户端不持有第二真相源
- 明确不做: 不做完整 Web UI、认证、多用户、远程写操作或公网监听

---

## 八、P6 运行时闭环（v0.7）

### TASK-601: 模型可见历史统一投影 ✅（commit `91fe432`）
- 目标 crate: protocol ⚠️串行、session、model-provider、agent-loop、harness-cli
- 内容: 区分模型表面事件与纯审计事件；压缩记录确定性的 replace-prefix 操作及来源事件；resume 统一从事件流投影模型历史
- 验收标准:
  1. user/assistant/tool_call/tool_result/compaction 可重放为与在线运行语义一致的模型消息，Hook 等审计工具调用不得混入
  2. 压缩替换不拆 tool_call/tool_result，非法来源、重复结果、缺失结果 fail-closed
  3. 旧 JSONL（含旧格式 CompactionApplied）仍可读取；新增协议字段和事件有序列化兼容测试
- 明确不做: 不改变 provider HTTP 协议；不修复历史文件；不新增持久化后端或外部依赖
- 依赖: TASK-504

### TASK-602: 根预算与子树 Token 用量账本 ✅（commit `9966fa8`）
- 目标 crate: protocol ⚠️串行、context、agent-loop、session
- 内容: provider usage 优先、估算兜底；记录 own/subtree/root remaining；嵌套 subagent 消耗归集根预算
- 验收标准: 主代理与两层子代理消费均落 Event；重放可恢复余额；超限在下一次采样前稳定拒绝且 runner/provider 零调用
- 明确不做: 不实现计费；不按 message 文本推断错误；不允许子策略扩大根预算
- 依赖: TASK-601

### TASK-603: 权限 epoch 与执行环境事实 ✅（commit `05d9df5`）
- 目标 crate: protocol ⚠️串行、sandbox-policy、sandbox-exec、approval、tools
- 内容: 审批绑定 policy epoch、权限配置摘要和 executor OS/home/workspace facts；状态变化使旧授权失效
- 验收标准: policy/workspace/target 任一变化后旧决定不可复用；未知或过期环境 fail-closed；授权与失效均留 Event
- 明确不做: 不实现远程执行协议；不缓存凭据；不改变 SandboxMode 三档语义
- 依赖: TASK-602

### TASK-604: 受监管 MCP registry ✅（commit `0e275c2`）
- 目标 crate: tools、agent-loop
- 内容: required/optional 服务状态机、发现宽限、connection generation、结构化错误、按工具输出限制与安全结果中间件
- 验收标准: optional 超时可降级而 required 失败拒绝启动；旧 generation 调用被拒；单服务失败不隐藏其他有效目录
- 明确不做: 不做 HTTP MCP/OAuth；不引入异步运行时；不执行服务端任意代码
- 依赖: TASK-603

### TASK-605: generation-aware RPC/SSE 连续性 ✅（commit `47f5144`）
- 目标 crate: protocol ⚠️串行、harness-cli
- 内容: follow-before-page、connection generation、Last-Event-ID 续传与序号缺口修复；服务端能力协商保持只读
- 验收标准: 首屏与并发追加无窗口丢失；断线补洞无重无漏；旧 generation、坏 cursor 和业务错误不自动无限重试
- 明确不做: 不开放远程写、审批或公网监听；不实现完整 Web UI/认证
- 依赖: TASK-604

### TASK-606: 持久化 Agent Team 协调层 ✅（完成，提交见 git 历史）
- 目标 crate: protocol ⚠️串行、session、agent-loop
- 内容: durable roster/mailbox、消息去重、带 revision/CAS 的任务 DAG、blockedBy 与 writeScopes 重叠告警
- 验收标准: 崩溃重放恢复团队状态；重复消息恰好投递一次；环依赖/旧 revision fail-closed；写范围冲突产生审计告警
- 明确不做: 不强制文件锁；不跨进程调度；不允许团队成员扩大父权限
- 依赖: TASK-605

### TASK-607: 可信插件清单与结果中间件 ✅（commit `bfbf03b`）
- 目标 crate: tools、agent-loop、harness-cli
- 内容: 本地插件 manifest、来源/哈希/能力声明校验；有效目录隔离加载；工具结果进模型前可检查、脱敏或拒绝
- 验收标准: 路径逃逸/哈希漂移/未声明能力拒绝；坏插件不遮蔽好插件；安全中间件缺席或失败时 fail-closed 并留 Event
- 明确不做: 不做远端市场/自动下载；不执行 shell hook；不引入新 workspace 外部依赖
- 依赖: TASK-606

---

## 九、P7 工具面扩展（v0.8）

> 来源：对标 `codex`（openai/codex）与 `DeepSeek-Harness` 的 2026-08 现状盘点（差距分析见会话记录）。
> 核心判断：P6 已收口可靠性与安全内核，缺的是"能让 harness 真正干活"的工具广度。
> 本节卡片已按 2026-08-31 实际交付修订（as-built）；与最初卡面的偏差以「落地注记」标明。

### TASK-701: 内置文件工具集 ✅（commit `89cd893`）
- 目标 crate: tools、harness-cli（装配/测试）
- 内容（as-built）: `fs_read` / `fs_write` / `fs_edit`（str_replace 语义）/ `fs_glob` / `fs_grep` 内置工具，以 canonical workspace root 为信任边界（词法穿越栅栏 + symlink 拒绝）；write/edit 强制 read-before-write；超大文件与超限结果集落 `.harness/spill`，结果只带预览 + locator
- 验收标准（达成）: 未读先写/编辑被 SandboxDenied 拒绝；edit 锚串不匹配或歧义时原文件零改动；glob/grep 结果超限落 spill 且 locator 可被 fs_read 取回全文；全部经 ToolRegistry schema 校验；路径越界被拒
- 明确不做: 不打包 ripgrep 等外部二进制（std 实现即可）；不做 diff/补丁格式；不新增持久化后端
- 落地注记: spill 在 tools 内自实现（依赖方向 tools 不依赖 session，未复用 session::SpillStore）；"越界路径被拒"由工具内边界守卫承担（tools 不依赖 sandbox-policy）；CAS 与原子替换由 TASK-804 在本卡工具上演进
- 依赖: 无

### TASK-702: 工具执行超时与循环防护 ✅（commit `16c9ae6`）
- 目标 crate: protocol ⚠️串行、tools、agent-loop
- 内容（as-built）: 新增稳定码 `ToolTimeout` 与 `ToolLoopDetected`；`ToolSpec.timeout_ms` 按工具 deadline，调度在独立线程限时等待（`run_with_deadline`），超时返回 ToolTimeout 且配对完整；agent-loop 可选 `LoopGuard`——同一工具连续等参调用先在结果中附加提醒、达到上限后不再触发 handler
- 验收标准（达成）: 注入挂死 handler 后超时返回 ToolTimeout 且配对完整；第 N+1 次重复调用被拒绝且 handler 零调用（测试锁定）；参数变化重置计数；未配置护栏/deadline 时行为与既有完全一致（增强性护栏，非 fail-closed 安全件）
- 明确不做: 不引入异步运行时（std 线程 + recv_timeout）；不并发调度；不按 message 文本判断超时
- 落地注记: "全局默认 deadline" 在 TASK-802 落地（`ToolRegistry::set_default_deadline`）；"超时不取消底层副作用"的局限由 TASK-802 升级为协作取消
- 依赖: 无（protocol 串行）

### TASK-703: 白名单代理 web_fetch 工具 ✅（commit `a0ff4d4`，断链修复 `2868fe9`）
- 目标 crate: tools、model-provider（HTTP 复用）、harness-cli（装配）
- 内容（as-built）: `web_fetch(url)` 工具 + `Fetcher` 物理通道抽象——仅 http/https；私网/回环字面量一律拒绝（SSRF）；主机白名单默认拒绝；重定向逐跳复检；内容超限 spill；生产适配层 `http_fetch_via_proxy`（禁自动重定向、硬字节上限、仅回环代理）
- 验收标准（达成）: 白名单外/私网/回环 fail-closed（工具层 SandboxDenied；代理层 NetworkAccessDenied）；重定向逃逸被拒；超限响应经 locator 取回
- 明确不做: 不做 web_search；不做 JS 渲染/浏览器自动化；不做响应缓存
- 落地注记: 首版存在装配断链（代理 allowlist 只含 provider 域名）——`2868fe9` 修复：`--fetch-allow` 主机同时注入代理 allowlist（`start_with_fetch_hosts`），代理补齐明文 GET/HEAD 转发与 Host 头一致性，并修复 Windows accept 非阻塞继承问题
- 依赖: 复用 P2 链路（TASK-203/206）

### TASK-704: turn 内 steer 与排队输入 ✅（commit `c01ca11`）
- 目标 crate: protocol ⚠️串行、agent-loop、harness-cli
- 内容（as-built）: `UserInputQueued` 事件承载运行中入队的 steer；模型表面投影将其视同 User 消息，并在工具批次未闭合时延迟出账（保住 provider 消息序）；agent-loop 以持久游标在每个采样轮边界吸收，跨 turn 残留由下一 turn 接管；`enqueue_input` API + `mark_queued_inputs_consumed`
- 验收标准（达成）: 运行中入队零丢失且不拆配对；轮边界按序可见；turn 间残留不丢；resume 与在线视图一致（投影即真相）；空白输入拒绝
- 明确不做: 不做多 turn 并行；不做采样中途抢占；不做跨进程注入
- 落地注记: CLI 使用面（`/steer` 命令）在 TASK-803 落地
- 依赖: 无（protocol 串行）

### TASK-705: 跨会话记忆投影 ✅（commit `7d6d5ea`）
- 目标 crate: session、agent-loop、harness-cli
- 内容（as-built）: `MemoryRecorded` 事件（同 id 后写覆盖）+ `MemoryContextInjected` 事件（加法式系统消息进模型表面）；`memory_write` 工具经 `ToolAudit::MemoryRecorded` 由 agent-loop 落事件；CLI 启动注入幂等，resume 重建与在线视图一致
- 验收标准（达成）: 记忆跨 resume/fork 重放恢复；注入不破坏模型表面投影一致性（TASK-601 不变量测试锁定）；注入预算超限 fail-closed
- 明确不做: 不做向量检索/嵌入模型；不做自动遗忘/衰减策略；不做跨用户共享
- 落地注记: "单条记忆大小受限"在 TASK-806 落地；作用域语义（不跨独立会话）在 TASK-806 明确为 LineageOnly
- 依赖: TASK-601

### TASK-706: Linux Landlock 生产后端 ✅（commit `e7ed7d2`；Linux 侧由 TASK-807 的 Ubuntu CI 实证）
- 目标 crate: sandbox-exec
- 内容（as-built）: `LandlockBackend` 实现 RestrictedBackend（ABI v1 文件系统规则）：子进程 `PR_SET_NO_NEW_PRIVS` 后声明全盘只读+执行，WorkspaceWrite 档额外授予工作区根全量文件权；未声明访问默认拒绝；无 Landlock 内核 fail-closed 不降级；`PlatformRestrictedBackend` 在 Linux 组合该后端，未知 sandbox 档位 fail-closed
- 验收标准: Linux 集成测试（已随卡交付，cfg(linux) 隔离）证明越界读写被拒、只读档内写被拒、未知档位 fail-closed；Windows 路径零回归（本机门禁覆盖）；denial 以失败结果留 Event
- 明确不做: 不做 Windows WFP 网络过滤（另立卡）；不改变 SandboxMode 语义；不做 macOS Seatbelt；Landlock v4 网络域暂不落地
- 允许依赖: `libc`（仅 Linux target；2026-08-30 卡面修订）
- 依赖: TASK-603

### P7 暂缓清单（明确不排卡，避免范围蔓延）
PTY 持久终端 / code-mode（模型编写代码编排工具调用，V8 或 worker 运行时）/ TUI / 多 provider 抽象 / MCP OAuth / 插件远端市场——均为产品面或与「无异步运行时、最小依赖」原则冲突；PTY/code-mode 与 TUI 的重估在 P8 出口评审后进行。

---

## 十、P8 安全与产品化收口（v0.9）

> 来源：P7 完成后的整体验收。当前单元/集成测试覆盖较强，但安全边界、真实 CLI 装配、跨平台证据和发布口径仍有缺口。
> 本节卡片已按 2026-08-31 实际交付修订（as-built）；跨平台 CI 已取得远程实证，真实模型冒烟因未提供 key 仍待验收。

### TASK-801: DNS 解析后目标钉扎与 SSRF 闭环 ✅（commit `48989d9`）
- 目标 crate: network-proxy（钉扎与校验的落地层）、tools（703 字面量 SSRF 检查沿用）、harness-cli（测试旗标）
- 内容（as-built）: 代理对 allowlist 内主机做 DNS 解析并**全量校验**每个解析结果（loopback、unspecified、RFC1918、CGNAT 100.64/10、link-local、组播、保留段、IPv4-mapped IPv6），任一命中即整体拒绝；随后只连接**已校验的 SocketAddr**（校验地址 = 连接地址，rebinding 无法绕过）；明文 GET/HEAD 在解析前先校验 Host 头与目标一致性
- 验收标准（达成）: `localhost` 解析到 `::1`/`127.0.0.1` 被拒且审计事件带 `forbidden_resolved_ip:<ip>`；rebinding 以结构证明——`resolve_and_pin` 返回的地址即唯一被连接地址（单测锁定）；Host 头不一致拒绝（`host_header_mismatch`）；解析失败拒绝（`resolution_failed`）；provider 链路与既有代理测试零回归
- 明确不做: 不实现递归 DNS 服务；不做通配域名白名单；不开放公网代理；不新增运行时外部依赖
- 落地注记: 测试用本地回环源站通过显式 `ProxyPolicy::allow_forbidden_targets()` 旗标放行（生产装配永不调用）；DNS 钉扎的落地层是代理而非 reqwest 自定义 resolver
- 依赖: TASK-703

### TASK-802: 工具 deadline 的真实取消与副作用收口 ✅（commit `e46d925`）
- 目标 crate: protocol ⚠️串行、tools、sandbox-exec、agent-loop
- 内容（as-built）: `CancellationToken` 协作取消——deadline 到期即取消，有副作用的 handler（fs_write/fs_edit）在入口/提交点 `check`，被取消后以稳定 ToolTimeout 拒绝继续（不强杀宿主线程，但副作用被闸住）；外部命令超时终止进程树——Windows 将受限子进程加入 `KILL_ON_JOB_CLOSE` Job Object 并 `TerminateJobObject`（Extended 限额结构长度实证为 144 字节），Linux 子进程 `setpgid` 自成进程组、超时 `killpg SIGKILL`；终止失败 fail-closed 返回明确错误；新增 `ToolExecutionTerminated { call_id, termination: DeadlineExceeded/Cancelled/ProcessTreeTerminated }` 结构化留痕；`ToolRegistry::set_default_deadline` 全局默认（spec.timeout_ms 优先）
- 验收标准（达成）: ToolTimeout 返回后 handler 提交点拒绝写文件（副作用计数不变，测试锁定）；Windows 超时进程被终止且约定退出码 124、快速返回（真实 cmd/ping 测试）；tool_call/tool_result 严格配对（终止事件 + 失败结果成对）；重复取消幂等；未配置 deadline 行为兼容
- 明确不做: 不引入异步运行时；不强杀任意宿主线程（协作取消 + 外部进程强终）；不把无法取消的 handler 伪装成"已终止"
- 依赖: TASK-702、TASK-706

### TASK-803: P7 能力的生产 CLI 装配 ✅（commit `7b23b77`）
- 目标 crate: tools、agent-loop、harness-cli
- 内容（as-built）: 真实 `chat` 装配 `FsToolSet`（`--workspace` 为唯一信任根，canonical 化缺失即拒启）、`--plugin-root` 显式插件目录（不自动信任工作区插件；隔离失败跳过且不遮蔽）、`ProductionResultMiddleware`（结果大小预算）、`LoopGuard` 默认值（3 提醒/8 拒绝）、`/steer` 命令与 `mark_queued_inputs_consumed`；`/tools` 与模型广告由 registry 实际注册驱动（新增 `ToolRegistry::names()`），未装配能力不广告
- 验收标准（达成）: 生产装配后模型可调用全部 fs_* 工具且路径绑定 --workspace；插件装配/隔离/guard 均在装配路径可测（guard 在位则小结果放行、超预算结果被截断替换；插件来源结果在 guard 缺席时 fail-closed 由 607/803 测试锁定）；/steer 入队落 UserInputQueued 事件、下一采样边界可见且不拆配对；CLI 无遗留输入线程（同步模型）
- 明确不做: 不做 TUI；不做远程插件市场；不做多 turn 并行；不自动信任工作区插件
- 依赖: TASK-607、TASK-701、TASK-704、TASK-802

### TASK-804: 文件写入 hash/CAS 与原子替换 ✅（commit `1b79f6e`）
- 目标 crate: protocol ⚠️串行、tools、harness-cli
- 内容（as-built）: `fs_read` 返回稳定内容摘要（fnv1a hex，与插件 hash 同算法）；`fs_write`/`fs_edit` 覆盖既有文件时必须携带 `expected_hash`，与当前内容不符返回新稳定码 `FileRevisionConflict`（只读 RPC 映射 409）且文件字节级零改动；写入采用同目录临时文件 + `sync_all` + `fs::rename` 原子替换，失败路径清理临时文件；rename-over 不跟随被替换的 symlink（校验后路径被换成 symlink 也只会替换链接项本身）
- 验收标准（达成）: 读取后被外部修改时旧 hash 写入返回 FileRevisionConflict 且零改动（测试锁定）；缺 expected_hash 覆盖既有文件返回 ToolArgsInvalid；成功/失败路径均无 `.ih-tmp-*` 残留；symlink 在解析时被拒（701 既有守卫），CAS 校验与 rename 组合收窄 TOCTOU
- 明确不做: 不实现通用 diff/merge；不强制文件锁；不覆盖用户的新修改；不引入新的持久化后端
- 落地注记: "Windows/Linux 均有平台测试"——fs 工具为跨平台同一实现，Windows 实测；Linux 由 CI 矩阵覆盖（同 807 依赖）
- 依赖: TASK-701、TASK-603

### TASK-805: Team 状态变更与冲突审计原子化 ✅（commit `c8fcdca`）
- 目标 crate: session、agent-loop（protocol 未改动）
- 内容（as-built）: `SessionStore::append_batch`（默认实现退化为逐条，供其他存储兼容）；JsonlSession 覆盖实现——先写 `.ih-pending` 回滚日志（记录批次前主文件长度）并 fsync，再单次写入整批并 fsync，成功后删除日志；**打开时发现残留日志即截断回滚整批**——任何崩溃点重放只见整批旧状态或整批新状态；ZstdSession 透传其 406 原生批次写；TeamCoordinator 的 create/update 与 write-scope 冲突审计合并为一个不可分割批次
- 验收标准（达成）: 回滚日志注入测试证明整批回滚（重放只剩提交前状态）；批次 seq 连续无缺口；CAS 旧 revision 继续稳定拒绝（606 既有测试）；JSONL 与 zstd 对批次语义一致（zstd 原生批次 + trait 统一）；SQLite 投影按事件流重建天然一致
- 明确不做: 不强制文件锁；不实现跨进程 agent 调度；不允许 Team 状态绕过 append-only 真相源
- 落地注记: 未改 protocol——原子性由存储层回滚日志达成，事件契约不变
- 依赖: TASK-606、TASK-406、TASK-407

### TASK-806: 持久记忆作用域、来源与防污染语义 ✅（commit `886e577`）
- 目标 crate: protocol ⚠️串行、session、agent-loop、harness-cli
- 内容（as-built）: 选择「不真正跨 session」分支——作用域定为 `MemoryScope::LineageOnly`（仅沿同一会话血脉 resume/fork 可见），并禁止任何文档宣称跨独立会话/用户共享；来源 `MemorySource::Model|Host` 落事件、在注入摘要中逐条标注（`[tags][source] text`）从而可审计；`MemoryRevoked` 幂等撤销（对不存在 id 无效果）；三重预算 fail-closed——单条写入上限（32KB）、总量上限（256KB）、注入摘要字符预算（16K）
- 验收标准（达成）: 撤销可重放且幂等（测试锁定）；来源可审计且注入摘要带来源标注；网页/插件文本不经 memory_write 管道无法成为持久系统提示（唯一写入口为 memory_write 工具，宿主写入走 Host 来源）；单条/总量/注入预算超限 fail-closed
- 明确不做: 不做向量数据库；不做跨用户共享；不自动上传记忆；不按 message 文本猜测可信度
- 落地注记: P7 卡 705 遗漏的「单条记忆大小受限」在本卡收口
- 依赖: TASK-705、TASK-602、TASK-607

### TASK-807: 跨平台 CI 与安全供应链门禁 ✅（实现 `9185617`；修复收口至 `06e4923`）
- 目标范围: `.github/workflows`、workspace 配置、sandbox-exec 测试
- 内容（as-built）: CI 触发改为**全分支** + PR；build/test/clippy 全部 `--all-features` 且 clippy `-D warnings`；Windows + Ubuntu 双矩阵——Ubuntu 真实执行 Landlock 越界测试（内核不支持时测试硬失败，不允许静默跳过冒充通过）；新增 MSRV job（workspace 声明 `rust-version = "1.85.0"`，全 crate 继承）；新增 cargo-deny 供应链 job（advisories/licenses/bans/sources，可复现配置 `deny.toml`，例外必须留证）
- 验收结果: GitHub Actions [run #13](https://github.com/BigDataDZ/ideal-harness/actions/runs/33353637643) 四项全绿——Windows、Ubuntu（真实执行 Landlock 越界测试）、MSRV 1.85、cargo-deny 均通过
- 明确不做: 不在 CI 使用真实 API key；不把不稳定公网测试设为单元门禁；不引入运行时外部依赖
- 依赖: TASK-801、TASK-802、TASK-804

### TASK-808: 真实仓库代码任务端到端验收 🟡（scripted 已完成；真实模型冒烟待 key）
- 目标 crate: harness-cli、tools、agent-loop；目标目录: tests/manual
- 内容（as-built）: 离线 scripted-provider 端到端场景走**生产装配**（register_chat_tools + 受限 exec + 审批 + 结果中间件 + 循环护栏）完成「fs_grep 定位 → fs_read 拿 hash → fs_edit CAS 修复 → exec 提权跑测试 → 完成」全链；断言最终文件内容、精确工具调用轨迹、ApprovalDecided 审计、turn 完成、无幽灵副作用；真实模型手动冒烟规程（含篡改复现 FileRevisionConflict、/steer、resume 验证）写入 tests/manual/p8-smoke.md
- 验收结果: 1（离线 scripted CI 回归）达成；3 达成（场景直接调用生产装配函数而非测试专用路径）；2 的规程已就绪，但 2026-08-31 当前环境未设置 `IDEAL_HARNESS_API_KEY`，执行记录待 key 持有者回填
- 明确不做: 不让 CI 依赖付费模型或公网；不自动修改真实用户仓库；不以单次成功替代故障注入测试
- 依赖: TASK-801~807

### TASK-809: 版本、文档与能力声明一致性 ✅（commit `54eb5a6`）
- 目标范围: Cargo workspace、README、CHANGELOG、ROADMAP
- 内容（as-built）: workspace 版本 0.2.0 → 0.9.0（对齐路线图）；CHANGELOG 增 0.9.0 段逐卡记录；README 区分「代码与自动化门禁完成」和「真实模型发布验收待 key」；本卡即文档同步提交
- 验收标准: README 每项声明有生产入口与测试对应；版本/CHANGELOG/路线图一致；Landlock 已链接远程实证，真实模型冒烟明确标为未完成而非宣称通过
- 明确不做: 不重写历史提交；不删除历史任务卡；不虚构 CI、性能或安全验证结果
- 依赖: TASK-808

### TASK-810: 协议解析 fuzz、长会话 soak 与资源上限 ✅（commit `588104e`）
- 目标 crate: protocol、model-provider、network-proxy、tools、session
- 内容（as-built）: 五个不可信解析边界的确定性 fuzz target（xorshift 字节级变异）——Event JSONL 解析、会话 replay、SSE 行解析、工具 dispatch 参数、代理请求头解析——断言只允许 Ok 或稳定 Err、绝不 panic；15 万事件 soak（批量写入 2.7s）覆盖 seq 连续性、模型表面配对、timeline 分页无重无漏；资源硬上限——fs/web spill 各自总量预算（64MB，超限稳定码拒绝且计入审计）、registry 分离线程上限（64，RAII 计数）、代理并发连接上限（256，超限 503 + NetworkAccessDenied 审计）
- 验收标准: 每个边界至少一个 fuzz target ✓（固定崩溃样本以回归测试形式随卡内联）；10 万级事件重放/投影/分页 ✓；spill/分离任务/代理连接上限 ✓ 留审计
- 明确不做: 不把 fuzz 随机性带入普通单元测试（全部固定种子）；不承诺未测平台；不新增运行时外部依赖
- 落地注记: fuzz 为仓库内确定性测试（非 cargo-fuzz/libFuzzer——避免新增工具链依赖）；真实模糊测试基础设施若需要另立卡
- 依赖: TASK-801、TASK-802、TASK-805

### P8 建议执行顺序（已按此执行）

- **安全串行链**：801 → 802 ✅
- **产品/一致性并行组**：803、804、805、806 ✅
- **发布证据链**：807 ✅ → 808 🟡 → 809 ✅；810 ✅
- **P8 出口判据**：801~807、809~810 完成 ✅；真实 CLI scripted 端到端已证 ✅；真实模型冒烟待 key 🟡；Windows/Linux 远程 CI 全绿 ✅；README、版本和生产能力一致 ✅

## 十一、P9 桌面客户端（v1.0）

> 推荐栈：Tauri 2 + React + TypeScript + Vite。Rust Harness 仍是唯一业务真相源，客户端只消费
> Event/RPC/SSE 投影；任何写操作、审批和密钥访问必须经过受控 Rust command，不向 WebView 暴露
> 任意文件系统、shell 或远程网络能力。Taro 仅在未来明确需要微信/支付宝小程序时另行评估。

### TASK-901: 桌面端架构决策、边界与最小骨架 ✅（commit `cdba3e3`）
- 目标范围: `docs/DESIGN-DECISIONS.md`、`AGENTS.md`、`docs/DEVELOPMENT.md`、`apps/desktop`
- 内容（as-built）: 新增 D25，确定 Tauri 仅是受限 UI 宿主、Event 是唯一真相源、命令面与投影面分离；修订“唯一 main”规则并登记桌面模块所有权；创建独立的 Tauri 2.11 + React 19 + TypeScript 7 + Vite 8 工程、响应式 Windows 启动页、严格 CSP、空权限 capability 和仅返回版本/安全边界的 `desktop_status` command；npm/Cargo 依赖均精确锁定
- 验收结果: `npm run typecheck`、`npm run build`、桌面 `cargo check/clippy/test --locked` 全绿（1 test），Rust 1.85.0 MSRV 实测通过；npm audit 0 漏洞；Windows `tauri dev` 实际启动且窗口进程 responding；原 Rust workspace fmt/clippy 与 313 项测试全绿
- review 结果: 2026-08-31 用户确认按推荐方案继续，D25、CLI/Tauri 双薄入口例外及共享 Host library 方向获准
- 允许新增依赖: Tauri 2 官方核心/构建依赖、React、TypeScript、Vite，以及仅用于 lint/test 的前端开发依赖；版本必须锁定并提交 lockfile
- 明确不做: 不实现聊天业务；不复制 agent-loop；不把现有只读 HTTP 接口扩成无鉴权写接口；不引入 Taro/Electron
- 依赖: P8 自动化门禁已完成；TASK-808 真实模型冒烟可并行，不阻塞客户端骨架

### TASK-902: 提取可复用 Host 装配层并保持 CLI 零回归 ✅
- 目标 crate: 新建 `harness-host`（名称可在 D25 review 时定稿）、harness-cli
- 内容: 将 provider、代理、工具注册、审批注入、会话恢复和 AgentLoop 生产装配从二进制入口提取为可复用 library；CLI 与桌面端只做参数/交互适配；依赖方向同步写入所有权地图
- 内容（as-built）: 新增 `harness-host` 作为唯一生产装配库；迁移 provider 代理、受限执行工具、会话恢复、记忆注入、工具 schema 和结果安全中间件；CLI 改用 `ProductionHost`，桌面状态检查复用同一 `HostConfig` 校验边界；同步所有权地图与依赖方向
- 验收结果: workspace fmt、严格 clippy、Rust 1.85 build、315 项测试全绿；桌面独立工程 fmt/clippy、Rust 1.85 check 与 2 项测试全绿；前端 typecheck/build 全绿、npm audit 0 漏洞；`protocol` 未改动，缺审批器和未知执行环境的 fail-closed 回归继续通过
- 验收标准: CLI 现有命令与 313+ 回归测试不变；CLI 与桌面测试使用同一生产构造器；缺 key、缺审批器、未知执行环境继续 fail-closed；不新增或修改 protocol wire 契约
- 明确不做: 不做多 provider 抽象；不把 UI 状态写入 SessionStore；不解析错误 message 做控制流
- 依赖: TASK-901

### TASK-903: Tauri 安全桥接与生命周期管理 ✅
- 目标范围: `apps/desktop/src-tauri`、harness-host
- 内容: 只暴露显式 command DTO（启动/停止 turn、steer、取消、审批响应、会话操作）；窗口关闭时有界取消子进程与代理；按 Tauri capability 限定窗口权限；所有 command 输入做 schema/路径/epoch 校验
- 内容（as-built）: `harness-host::DesktopBridge` 集中校验 command generation、permission epoch、Host 安全配置、工作区 canonical 路径、会话 ID/边界、活动 turn 与审批执行代际；Tauri 仅注册 status/start/stop/cancel/steer/approval/session 七个显式 command，未知字段由 DTO 拒绝；取消或窗口销毁会取消工具令牌、关闭代理 Host 并推进安全代际；capability 继续保持零插件权限
- 验收结果: 旧 generation、旧 epoch、工作区/会话逃逸、Host/审批缺席、错误 turn、执行代际漂移和窗口关闭后调用均 fail-closed；返回 WebView 的错误只含稳定 code 与静态安全文案；Host 生命周期测试证明取消与 shutdown 均触发；workspace 318 项测试、桌面 3 项测试、严格 clippy、Rust 1.85、前端 typecheck/build 和 npm audit 全绿
- 验收标准: 未声明 command、路径逃逸、过期审批、旧 generation、窗口关闭后的调用全部 fail-closed；API key、审批内容和工具敏感结果不进入前端日志；生命周期集成测试证明无遗留代理/子进程
- 允许新增依赖: Tauri 官方 dialog/process/event 等插件仅按最小能力逐个列入；每个插件需在 PR 中给出权限说明
- 明确不做: 不开放任意 shell；不开放通用文件系统 API；不允许加载远程页面；不新增 loopback 写服务
- 依赖: TASK-902

### TASK-904: Event/SSE 投影状态库与断线补洞 ⬜
- 目标范围: `apps/desktop/src/lib/projection`、protocol 生成/手写 DTO 适配层
- 内容: 前端状态完全由 timeline + SSE 事件归约得到；实现 `Last-Event-ID`、connection generation、断线重连、gap repair、重复事件幂等和分页水位；本地仅保存非权威 UI 偏好
- 验收标准: 乱序、重复、断流、服务重启和分页并发的确定性测试全绿；重连后视图与 SessionStore replay 结果一致；未知事件可降级展示但不得静默改写状态
- 允许新增依赖: Zustand、TanStack Query；不得引入第二套持久业务数据库
- 明确不做: 不在浏览器状态中补造 Event；不乐观确认审批/工具成功；不缓存 API key
- 依赖: TASK-901、TASK-903、D18

### TASK-905: 会话导航、Timeline 与诊断面板 ⬜
- 目标范围: `apps/desktop/src/features/sessions`、`apps/desktop/src/features/timeline`
- 内容: 会话列表、新建/resume/fork/revert 入口，turn 状态、Event 时间线、错误码、Token 用量、Agent Team 状态和连接代际展示；错误展示使用 code 决定交互，message 只用于说明
- 验收标准: 空状态/加载/断线/坏会话/无权限均有明确 UI；fork/revert 必须二次确认且结果由事件回执确认；键盘导航和基础无障碍检查通过；快照覆盖主要状态
- 允许新增依赖: shadcn/ui（源码组件）或 Ant Design 二选一，由 TASK-901 锁定；不得并存两套组件库
- 明确不做: 不提供事件删除/篡改；不把客户端缓存作为会话列表真相源
- 依赖: TASK-904

### TASK-906: 流式对话、工具调用卡片与 Markdown 展示 ⬜
- 目标范围: `apps/desktop/src/features/chat`
- 内容: 用户输入、流式 assistant 文本、取消、steer、resume；tool_call/tool_result 成对卡片、稳定 ErrorCode、耗时与审计状态展示；Markdown/代码块安全渲染
- 验收标准: 流式中断可恢复且不重复文本；工具调用/结果永不拆对；Markdown 禁止原始 HTML、脚本和危险链接协议；长消息和高频 token 更新不卡死主线程；组件与端到端测试覆盖成功/拒绝/超时/取消
- 允许新增依赖: `react-markdown` 及最小安全插件；高亮库需按需加载并记录包体影响
- 明确不做: 不执行模型生成的 HTML/JS；不在 UI 猜测工具成功；首版不做语音和多窗口并行 turn
- 依赖: TASK-904、TASK-905

### TASK-907: 审批中心、工作区文件树与安全 Diff ⬜
- 目标范围: `apps/desktop/src/features/approval`、`apps/desktop/src/features/workspace`
- 内容: 展示命令、工作区、SandboxMode、权限 epoch、执行环境和风险原因；允许明确批准/拒绝；提供只读文件树、文件预览和变更 Diff，写入仍只能经 harness 工具 CAS 路径完成
- 验收标准: 审批服务/窗口不在场默认拒绝；过期审批不可点击复用；批准前完整展示实际参数；路径越界与 symlink 逃逸测试通过；Diff 可定位对应 Event 和 expected_hash
- 允许新增依赖: Monaco Editor 仅用于只读代码/Diff，必须懒加载；若包体预算不达标则退回轻量 Diff 组件
- 明确不做: 不提供绕过工具层的保存按钮；不嵌入任意交互 shell；不允许批量“永久批准”
- 依赖: TASK-903、TASK-906

### TASK-908: Provider 设置与系统密钥存储 ⬜
- 目标范围: `apps/desktop/src/features/settings`、`apps/desktop/src-tauri`
- 内容: 配置 base URL、model、fetch allowlist 和非敏感偏好；API key 写入操作系统安全存储，Rust 侧按需读取，WebView 永远拿不到明文；配置变更触发新的 generation/权限事实
- 验收标准: key 不出现在 Event、日志、崩溃报告、前端状态和导出文件；安全存储不可用时拒绝保存而非降级明文；provider 连通性测试区分认证/网络/超时稳定码；删除 key 可验证生效
- 允许新增依赖: Tauri 官方 Stronghold 插件或经 review 的系统 keyring crate 二选一，不得自行加密后落普通文件
- 明确不做: 不同步云端配置；不在 UI 展示完整 key；不默认放宽 fetch allowlist
- 依赖: TASK-903

### TASK-909: 桌面 E2E、安装包、签名与发布门禁 ⬜
- 目标范围: `apps/desktop`、`.github/workflows`、README、CHANGELOG
- 内容: scripted-provider 驱动桌面 E2E，覆盖新建会话→对话→工具卡→审批→文件 Diff→完成→重启恢复；生成 Windows MSI/NSIS，预留 macOS/Linux 矩阵；记录包体、冷启动和长会话性能预算
- 验收标准: Windows 安装/卸载/升级冒烟通过；前端 lint/typecheck/unit/E2E、Rust fmt/clippy/test、Tauri build 全部成为 CI 硬门禁；产物生成 SBOM 与校验和；正式发布必须配置代码签名，未配置时只允许生成标记为 unsigned 的内部测试包
- 允许新增依赖: Playwright 或 WebdriverIO 二选一用于 E2E；打包/签名仅使用 Tauri 官方支持链路
- 明确不做: 不自动发布未签名正式版本；不在 CI 使用真实模型 key；不因 UI 测试跳过现有 Rust 安全门禁
- 依赖: TASK-905、TASK-906、TASK-907、TASK-908

### P9 建议执行顺序

- **架构串行链**：901（含 D22/规范 review）→ 902 → 903
- **投影与产品链**：903 → 904 → 905 → 906 → 907
- **可并行项**：908 可在 903 后与 904~907 并行；909 在 905~908 完成后收口
- **P9 出口判据**：Windows 安装包可独立安装运行；scripted 桌面 E2E 全绿；真实模型完成一次代码任务冒烟；客户端状态可由事件流完全重建；审批/密钥/路径边界安全测试全绿；README、版本与能力一致

## 十二、质量门禁演进（随阶段收紧）

| 门禁 | P0 现在 | P1 起 | P2 起 | P3 起 |
|---|---|---|---|---|
| build+test 全绿 | ✅ | ✅ | ✅ | ✅ |
| clippy 无警告 | 建议 | **强制 `-D warnings`** | ✅ | ✅ |
| 故障注入测试 | mock 层面 | provider 必须有 | 沙箱拒绝路径必须有 | 压缩恢复路径必须有 |
| 属性测试 | — | — | — | 配对完整性必须 |
| 审计事件覆盖 | — | — | 网络拒绝必须落事件 | 自动压缩必须落事件 |

P9 新增硬门禁：前端 format/lint/typecheck/unit test、scripted 桌面 E2E、Tauri Windows build、
依赖审计与安装包校验；任一门禁不得替代或弱化既有 Rust workspace 门禁。

## 十三、规范自身的演进规则

1. 改 `DEVELOPMENT.md`/`AGENTS.md` 的 PR 必须在标题加 `[spec]` 前缀，人工 review
2. 新 crate 入册：PR 同时更新所有权地图（AGENTS.md §1）与本文档依赖图
3. 任务卡完成后：在本文件对应卡片打 ✅ 并附 PR 链接，禁止删除历史卡片
4. 阶段出口评审：人类守护者按「出口判据」逐条打勾后才开下一阶段的卡

## 十四、给协调者的并行调度建议

- **可并行组**：{102, 104} 与 {201} 与 {403} 互不触碰
- **P7 并行组**：{701, 703, 705, 706} 可并行；{702 → 704} 是 protocol 串行链，必须一张卡走到底
- **P8 调度**：先 801 → 802；再并行 {803, 804, 805, 806}（protocol 卡互斥）；最后 807 → 808 → 809，810 可在 807 后并行
- **P9 调度**：901 → 902 → 903 严格串行；904 → 905 → 906 → 907 为主链，908 在 903 后并行，909 最后收口
- **串行链**：101 → 103 → 303（协议→闭环→自愈，一条线一个人/代理跟到底）
- 每个智能体会话结束必须产出 AGENTS.md §6 汇报；连续两次汇报缺测试证据的智能体暂停派卡
