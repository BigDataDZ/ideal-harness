# DESIGN-DECISIONS —— 对标 DSH/Codex 的设计决策与注意点（ADR）

> 本文件回答一个问题：**每个架构决策从哪来、跟谁不同、会踩什么坑**。
> 双库实证来源见姊妹报告 `D:\ds\harness-comparison-report.md`；
> 本文自洽可读，不依赖外部文件。

## 一、决策对照表

| # | 决策点 | OpenAI Codex 做法 | DeepSeek Harness 做法 | **本项目选择** | 为什么 / 注意点 |
|---|---|---|---|---|---|
| D1 | 核心语言 | Rust（~百 crate workspace） | TypeScript（227 包 monorepo） | **Rust** | 学 Codex：类型系统在边界兜底、单二进制分发。⚠️ 注意：本机 crates.io 可能被沙箱拦截，统一用 `cargo build --offline`（依赖已缓存） |
| D2 | 模块粒度 | ❌ core 巨石化（session/mod.rs 3700 行） | ❌ 过度碎片化（YAML patch 行覆盖无深合并） | **~10 个粗粒度 crate，显式 Cargo 依赖组合** | 反两个极端。⚠️ 单文件 >400 行即拆分信号；禁止引入任何 YAML patch 组合机制 |
| D3 | 协议组织 | ✅ 独立 protocol crate，多客户端共用 | 协议散布包间 | **protocol-first，唯一契约 crate** | 学 Codex。⚠️ 涉协议任务卡必须串行；改契约需同步全部消费方序列化测试 |
| D4 | 错误处理 | ErrorCode 协议化 + 边界 thiserror/内部 anyhow | HarnessError 稳定 code + errorChain，63 具体类 | **ErrorCode 枚举路由** | 两家共识。⚠️ 铁律：控制流只匹配 code；DSH 曾证明解析 message 字符串是事故源 |
| D5 | 文件沙箱抽象 | SandboxPolicy(writable_roots)，.git 强制只读防提权 | ✅ 三层贯穿：一个 SandboxMode 枚举驱动 fs 栅栏/OS 沙箱/审批流/schema 广告 | **学 DSH：SandboxMode 三档单一抽象** | 抽象一致性靠类型而非文档。⚠️ 我们已实测踩坑：ReadOnly 分支漏判被测试抓出——每档模式必须有独立分支与独立测试 |
| D6 | OS 级隔离 | ✅ Landlock/seccomp(独立进程)+Seatbelt+CreateRestrictedToken+WFP | bwrap/Landlock/Seatbelt+koffi 调 RestrictedToken | **学两家共识：Windows 用 RestrictedToken，Linux 用 Landlock** | ⚠️ P2 才做（TASK-202）；先留 trait 位避免骨架期过度设计 |
| D7 | 网络控制 | ✅ network-proxy 白名单代理，默认断网 | ❌ 缺失同级机制 | **学 Codex：白名单代理进程** | 这是对比中发现 DSH 最大缺口，模型外传数据比读文件更危险。TASK-203 |
| D8 | 审批模型 | 前置策略制：AskForApproval 四档+Starlark 静态预裁决 | 逐调用提权制：justification 成对校验 fail-closed | **逐调用提权制（学 DSH）+ 静态预裁决思想（学 Codex，P2 后补）** | ⚠️ 提权参数必须成对校验（裸权限/孤儿理由都非法）；无审批服务=拒绝，绝不放行 |
| D9 | 权限出口可见性 | 审批由策略决定，schema 相对静态 | ✅ 提权字段动态注入工具 schema（受限后端挂载才广告） | **学 DSH：动态 schema 广告**（TASK-205） | 模型看得见出口才不会反复撞墙。⚠️ 广告逻辑与沙箱后端挂载状态耦合，需集成测试 |
| D10 | 会话持久化 | rollout JSONL+检查点重建 | ✅ 事件溯源 JSONL(zstd)+fork 种子复制 | **事件溯源 JSONL**（两家共识收敛解） | ⚠️ 坏行必须报错不能静默跳过（审计优先）；zstd 推迟到 TASK-403 |
| D11 | 上下文压缩 | ContextManager+预采样压缩+AutoCompactWindow | ✅ 双触发（压力阈值+溢出报错）+强制压缩后自动 retry+两段式裁剪 | **学 DSH：双触发+自动重试**（TASK-303 填 TODO 挂点） | ⚠️ 铁律：裁剪永不拆散 tool_call/result 配对（P3 起属性测试强制） |
| D12 | 测试基建 | ~600 *_tests.rs+insta 快照 741 | ✅ LLM mock 故障注入服务器+录制回放+100% 覆盖门禁 | **学 DSH 思想：mock 故障注入为必选项**；快照暂不引入 | ⚠️ 凡依赖 trait（ModelProvider/Approver）的逻辑必须测失败路径；临时文件用 temp_dir+进程 id，不引 tempfile 依赖 |
| D13 | UI/客户端 | TUI/app-server/MCP-server 三前端说同一协议 | Web 纯投影+SSE 补洞 repairGap | **UI 是纯投影**（两家共识） | 客户端不得持有第二真相源；P5 才做 |
| D14 | 模型可见历史 | rollout 记录结构化模型输入并维护压缩 lineage | surface event 与 log-only event 分离，`surfaceOp/sourceEventSeqs` 可重建 | **事件流派生唯一 Model Surface**（TASK-601） | 审计工具事件不得误入模型历史；压缩必须表达确定性的 replace-prefix 操作；旧事件缺元数据时兼容但不得伪造精确来源 |
| D15 | 层级预算 | 根目标统一计算主代理与嵌套 subagent 消耗 | 子任务声明预算并持久化 delegation depth | **根预算账本 + 子树用量**（TASK-602） | 准入额度不是实际消费；每次模型 usage 必须落 Event，子代理不能绕过根预算 |
| D16 | 权限时效 | Guardian 判定绑定当前权限状态；远端执行使用目标机真实环境 | host facts 带 generation，连接重建后替换瞬态状态 | **policy epoch + executor facts**（TASK-603） | 策略、工作区或执行目标变化后旧审批立即失效；未知目标环境 fail-closed |
| D17 | MCP 运行时 | required/optional 发现宽限、结构化错误、按工具限额与动态刷新 | connection-owned transport + generation guard | **受监管 MCP registry**（TASK-604） | 可选服务超时只降级，必需服务失败则拒绝；旧 generation 的工具句柄不可调用 |
| D18 | RPC 连续性 | app-server 线程恢复、能力协商与服务端请求 | follow-before-page、连接 generation、gap tail repair | **先强化只读流，再开放写 RPC**（TASK-605） | `last_seq` 与 generation 双校验；只对传输中断自动续接，业务错误不无限重试 |
| D19 | Agent Team | subagent 并行但提醒写冲突与 token 成本 | 持久 mailbox + CAS task DAG + blockedBy/writeScopes | **事件溯源的轻量 Team Coordinator**（TASK-606） | 首版只做任务所有权与写范围重叠告警，不做强制文件锁和跨进程调度 |
| D20 | 插件供应链 | plugin catalog 统一打包 skills/MCP/hooks，信任后启用 | 能力经容器组合与宿主连接提供 | **可信清单 + 能力声明 + 结果中间件**（TASK-607） | 先校验来源/哈希/权限再加载；插件失败不能遮蔽其他有效插件，不执行任意 shell hook |
| D21 | 工具执行护栏 | 每命令超时与 sandbox 拒绝重试编排 | timeout-policy（挂死即超时错误）+ repeat-tool-reminder（3/5/8 次提醒） | **deadline 线程限时 + 可选 LoopGuard 先提醒后拒绝**（TASK-702） | 超时不取消底层副作用（handler 是分离线程）；护栏缺席时行为不变（增强件非 fail-closed 安全件）；拒绝必须保持 tool_call/result 配对 |
| D22 | turn 内 steer | queued inputs + steering 中断点（feature flag） | agent.inject() 任意时刻注入，轮边界生效 | **事件化 UserInputQueued + 采样轮边界按游标吸收**（TASK-704） | 投影在工具批次未闭合时延迟出账（provider 消息序合法）；resume 与在线视图必须一致；不做采样中途抢占 |
| D23 | 工具面广度 | shell/unified_exec/apply_patch/web_search 为核心，文件编辑靠 patch | 内置 fs/搜索/终端/web 全家桶 + read-before-write 观察策略 | **声明式内置文件工具 + 白名单代理内 web_fetch**（TASK-701/703） | 写前必读 fail-closed；fetch 拒私网/回环且逐跳复检重定向；出网只经 CONNECT/明文白名单代理，fetch 主机必须同时进代理 allowlist |
| D24 | 跨平台沙箱后端 | Seatbelt/Landlock/bwrap/AppContainer+WFP 全平台矩阵 | bwrap/Landlock/Seatbelt/Windows ACL token | **Windows 受限 token + Linux Landlock（TASK-706）**，其余平台 fail-closed 拒执行 | 无 Landlock 的内核拒绝执行不降级；未知 sandbox 档位 fail-closed；网络域限制（v4 NET）暂不落地 |


## 二、本项目独有的环境注意点（两家文档都没有的）

1. **离线构建**：沙箱环境可能拦截 crates.io——一律 `cargo build/test --offline`；新增依赖前先确认本机缓存（`~/.cargo/registry/cache`）有对应版本，否则任务卡里显式声明需要联网
2. **Windows 路径语义**：词法栅栏测试中 `/ws/a.txt` 在 Windows 上 `starts_with("/ws")` 成立但语义脆弱；生产实现必须叠加 canonical path + dev/ino 身份校验（DSH 的 containment.ts 是参照实现）
3. **PowerShell stderr 误报**：`cargo ... 2>&1` 重定向会让 pwsh 报 `[exit code: 1]` 即使构建成功——以输出中的 `Finished` 行为准，别被误导去"修"没坏的东西
4. **serde 命名契约对齐**：Event 用 snake_case tag；SandboxMode 序列化为 kebab-case（`workspace-write`）——与业界字符串契约对齐便于未来互操作，已有测试锁定
5. **AGENTS.md 自动加载**：本仓库被 DSH 类 harness 打开时 AGENTS.md 会自动注入上下文——所以红线写在那里而不是注释里；修改它等同改规范流程（[spec] 前缀 + 人工 review）

## 三、"刻意不同"清单（review 时重点盯）

与两家都不同的自主决策只有两处，均需在 PR 中额外论证：
1. **D2 组合方式**：既不用 Cargo workspace 细到 90+（Codex），也不用插件容器+声明式 patch（DSH），而是固定 ~10 crate 显式依赖——新增 crate 属于破坏性结构变更
2. **D8 审批时机**：混合两家而非全盘照搬——骨架期先用逐调用制（简单可测），静态预裁决作为 P2 优化叠加，两者共存时预裁决先行短路

## 四、维护规则

- 新增架构决策必须在本表加行，注明对标来源（学谁/不同于谁/自创）
- 发现对标项目新版本有值得抄的设计：开 `[spec]` PR 更新此表后再开任务卡
