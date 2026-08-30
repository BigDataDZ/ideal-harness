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
| P6 | v0.7 🚧 | **运行时闭环**：忠实重放、层级预算、权限时效与受监管扩展 | resume 与在线上下文等价；预算/权限/连接状态不可被旧状态绕过 |

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

### TASK-605: generation-aware RPC/SSE 连续性 ✅（完成，提交见 git 历史）
- 目标 crate: protocol ⚠️串行、harness-cli
- 内容: follow-before-page、connection generation、Last-Event-ID 续传与序号缺口修复；服务端能力协商保持只读
- 验收标准: 首屏与并发追加无窗口丢失；断线补洞无重无漏；旧 generation、坏 cursor 和业务错误不自动无限重试
- 明确不做: 不开放远程写、审批或公网监听；不实现完整 Web UI/认证
- 依赖: TASK-604

### TASK-606: 持久化 Agent Team 协调层
- 目标 crate: protocol ⚠️串行、session、agent-loop
- 内容: durable roster/mailbox、消息去重、带 revision/CAS 的任务 DAG、blockedBy 与 writeScopes 重叠告警
- 验收标准: 崩溃重放恢复团队状态；重复消息恰好投递一次；环依赖/旧 revision fail-closed；写范围冲突产生审计告警
- 明确不做: 不强制文件锁；不跨进程调度；不允许团队成员扩大父权限
- 依赖: TASK-605

### TASK-607: 可信插件清单与结果中间件
- 目标 crate: tools、agent-loop、harness-cli
- 内容: 本地插件 manifest、来源/哈希/能力声明校验；有效目录隔离加载；工具结果进模型前可检查、脱敏或拒绝
- 验收标准: 路径逃逸/哈希漂移/未声明能力拒绝；坏插件不遮蔽好插件；安全中间件缺席或失败时 fail-closed 并留 Event
- 明确不做: 不做远端市场/自动下载；不执行 shell hook；不引入新 workspace 外部依赖
- 依赖: TASK-606

---

## 九、质量门禁演进（随阶段收紧）

| 门禁 | P0 现在 | P1 起 | P2 起 | P3 起 |
|---|---|---|---|---|
| build+test 全绿 | ✅ | ✅ | ✅ | ✅ |
| clippy 无警告 | 建议 | **强制 `-D warnings`** | ✅ | ✅ |
| 故障注入测试 | mock 层面 | provider 必须有 | 沙箱拒绝路径必须有 | 压缩恢复路径必须有 |
| 属性测试 | — | — | — | 配对完整性必须 |
| 审计事件覆盖 | — | — | 网络拒绝必须落事件 | 自动压缩必须落事件 |

## 十、规范自身的演进规则

1. 改 `DEVELOPMENT.md`/`AGENTS.md` 的 PR 必须在标题加 `[spec]` 前缀，人工 review
2. 新 crate 入册：PR 同时更新所有权地图（AGENTS.md §1）与本文档依赖图
3. 任务卡完成后：在本文件对应卡片打 ✅ 并附 PR 链接，禁止删除历史卡片
4. 阶段出口评审：人类守护者按「出口判据」逐条打勾后才开下一阶段的卡

## 十一、给协调者的并行调度建议

- **可并行组**：{102, 104} 与 {201} 与 {403} 互不触碰
- **串行链**：101 → 103 → 303（协议→闭环→自愈，一条线一个人/代理跟到底）
- 每个智能体会话结束必须产出 AGENTS.md §6 汇报；连续两次汇报缺测试证据的智能体暂停派卡
