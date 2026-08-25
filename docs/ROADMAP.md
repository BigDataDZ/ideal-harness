# ideal-harness 开发演进路线图（ROADMAP）

> 版本 v0.1 | 配套规范：`AGENTS.md`（操作入口）、`DEVELOPMENT.md`（完整规范）
> 本文件是任务卡的唯一来源；领取前确认无重叠认领。

---

## 一、阶段总览

| 阶段 | 版本 | 主题 | 出口判据 |
|---|---|---|---|
| P0 | v0.1 ✅ | 架构骨架 + 协同规范 | 8 crate / 29 测试全绿（已完成） |
| P1 | v0.2 | **可对话 MVP**：接真实 LLM + 工具调用闭环 | 用真实 API key 完成带工具调用的多轮对话 |
| P2 | v0.3 | **安全纵深**：OS 沙箱 + 网络代理 + 人工审批 | 外部命令全部经受限进程池执行；默认断网 |
| P3 | v0.4 | **上下文工程**：压缩/spill/token 计量 | 长会话不触顶；溢出自动恢复有测试 |
| P4 | v0.5 | **会话产品化**：resume/fork/投影存储 | 断点续聊；fork 分支独立演化 |
| P5 | v0.6+ | **扩展生态**：MCP/skill/hooks/Web UI | 第三方工具经 MCP 接入 |

并行规则：**同一阶段内不同 crate 的任务卡可由多个智能体并行认领；涉及 protocol 的任务串行**（契约冻结原则）。

---

## 二、P1 可对话 MVP（v0.2）

### TASK-101: 协议扩展——模型流式契约
- 目标 crate: protocol ⚠️串行
- 内容：Event 增加 `ModelChunkReceived`；新增 `ModelCallSpec`（model/base_url/temperature）；ErrorCode 已含 ModelStreamBroken 无需改
- 验收：序列化往返测试；旧 JSONL 文件仍可重放（向后兼容证明）
- 明确不做：不引入认证字段（属 provider 层）

### TASK-102: 新 crate model-provider——OpenAI 兼容客户端
- 目标 crate: model-provider（新建，允许依赖 protocol；加入所有权地图）
- 内容：HTTP POST + SSE 流式解析；错误映射：HTTP 4xx→ToolArgsInvalid 类比归 Internal，超限→ContextWindowExceeded，断流→ModelStreamBroken；API key 读环境变量 `IDEAL_HARNESS_API_KEY`
- 验收：mock HTTP server 三种故障注入测试（超时/截断/非 JSON 行）
- 明确不做：不做多 provider 抽象层（一个够用，接口留 trait）

### TASK-103: agent-loop 工具调用闭环
- 目标 crate: agent-loop（依赖 model-provider 加入地图）
- 内容：采样返回 tool_call → registry.dispatch → ToolResultAdded 追加 → 继续采样直至模型给出文本答复（最大轮次保护，防死循环）
- 验收：mock provider 测试「文本→工具→文本」三段序列；超过 max_tool_rounds 强制终结并留 Event
- 依赖：TASK-101

### TASK-104: cli chat 子命令
- 目标 crate: harness-cli
- 内容：`ideal-harness chat` 进入交互循环（stdin 读行）；`--session <path>` 复用既有 JSONL；Ctrl+C 优雅中止当前 turn 留 TurnAborted
- 验收：手动冒烟脚本记录到 tests/manual/chat-smoke.md
- 依赖：TASK-103

---

## 三、P2 安全纵深（v0.3）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-201 | tools | 完整 JSON Schema 校验器（type/enum/items/additionalProperties） | 对 DSH 风格 schema 的兼容测试集 |
| TASK-202 | sandbox-exec(新) | 受限执行进程池：Windows CreateRestrictedToken 优先，预留 Landlock trait 位 | 外部命令不在主进程内执行的架构测试 |
| TASK-203 | network-proxy(新) | 白名单代理进程；provider 域名默认放行，其余默认拒绝 | 断网默认态测试；白名单外连接被拒且落审计事件 |
| TASK-204 | approval | CLI 人工审批通道（终端 y/n）+ Approver trait 实现 | fail-closed 全分支已有测试基础上补集成测试 |
| TASK-205 | tools | 提权出口动态广告：仅受限后端挂载时注入 escalation 字段 | 只读模式下 schema 含提权字段、全开放模式不含 |

## 四、P3 上下文工程（v0.4）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-301 | context | TokenMeter：以 provider usage 为锚，启发式兜底 | usage 缺失时降级路径测试 |
| TASK-302 | context+agent-loop | 两段式压缩：ToolResultPruner → LLM 摘要替换区间 | **配对完整性属性测试**（随机裁剪不拆对） |
| TASK-303 | agent-loop | 溢出强制压缩后自动重试（填现有 TODO 挂点） | 注入 ContextWindowExceeded 后观察压缩事件+成功重试 |
| TASK-304 | session | spill：超长工具结果全文落盘，事件中只存预览+locator | 取回句柄 roundtrip 测试 |

## 五、P4 会话产品化（v0.5）

| 任务卡 | 目标 crate | 核心内容 | 关键验收 |
|---|---|---|---|
| TASK-401 | harness-cli | `resume` / `fork` 子命令 | fork 后两会话独立追加互不影响 |
| TASK-402 | session | SQLite 投影 + write-behind 编排 | 投影查询与 JSONL 重放一致性测试 |
| TASK-403 | session | zstd 帧压缩（可选依赖，任务卡批准后引入） | 新旧格式互读迁移测试 |
| TASK-404 | agent-loop(子模块) | 进程内 subagent 骨架 + report 回传事件 | 子代理失败不污染父会话 |

## 六、P5 扩展生态（v0.6+，方向性）

- MCP client：第三方工具经 stdio 接入 ToolRegistry（TASK-501）
- Skill 目录发现：`.harness/skills` YAML frontmatter + 热重载（TASK-502）
- Hook 生命周期点：pre/post_tool_use 最小集（TASK-503）
- Web 投影客户端：RPC+SSE，UI 零业务逻辑（TASK-504）

---

## 七、质量门禁演进（随阶段收紧）

| 门禁 | P0 现在 | P1 起 | P2 起 | P3 起 |
|---|---|---|---|---|
| build+test 全绿 | ✅ | ✅ | ✅ | ✅ |
| clippy 无警告 | 建议 | **强制 `-D warnings`** | ✅ | ✅ |
| 故障注入测试 | mock 层面 | provider 必须有 | 沙箱拒绝路径必须有 | 压缩恢复路径必须有 |
| 属性测试 | — | — | — | 配对完整性必须 |
| 审计事件覆盖 | — | — | 网络拒绝必须落事件 | 自动压缩必须落事件 |

## 八、规范自身的演进规则

1. 改 `DEVELOPMENT.md`/`AGENTS.md` 的 PR 必须在标题加 `[spec]` 前缀，人工 review
2. 新 crate 入册：PR 同时更新所有权地图（AGENTS.md §1）与本文档依赖图
3. 任务卡完成后：在本文件对应卡片打 ✅ 并附 PR 链接，禁止删除历史卡片
4. 阶段出口评审：人类守护者按「出口判据」逐条打勾后才开下一阶段的卡

## 九、给协调者的并行调度建议

- **可并行组**：{102, 104} 与 {201} 与 {403} 互不触碰
- **串行链**：101 → 103 → 303（协议→闭环→自愈，一条线一个人/代理跟到底）
- 每个智能体会话结束必须产出 AGENTS.md §6 汇报；连续两次汇报缺测试证据的智能体暂停派卡
