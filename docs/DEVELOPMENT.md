# ideal-harness 开发规范（完整版）

> 入口读者请先看根目录 `AGENTS.md`；本文件是完整规范，冲突时以本文件为准。
> **架构决策的对标依据**（与 Codex/DSH 的异同及注意点）见 `docs/DESIGN-DECISIONS.md`；
> 任务卡唯一来源见 `docs/ROADMAP.md`。
> 版本：v0.1（随原型建立） | 适用范围：人类贡献者 + AI 智能体

---

## 1. 工程宪法

1. **Protocol-first**：`crates/protocol` 是系统唯一契约。任何跨 crate/跨进程数据结构必须定义在协议层；客户端（TUI/Web/IDE）只是事件流的投影，不得持有第二真相源。
2. **单一抽象贯穿**：沙箱语义只有 `SandboxMode` 一个枚举。新增安全相关概念必须挂接到它，而不是另起炉灶。
3. **fail-closed 默认**：所有"服务不在场/参数不全/路径不明"的分支走向拒绝，绝不走向放行。
4. **事件溯源**：会话状态唯一来源是 append-only JSONL 事件流。恢复=fork=重放；一切自动行为留痕。
5. **错误按 code 路由**：`ErrorCode` 是机器契约，message 只供人读。禁止 `contains("...")` 式控制流。
6. **粗粒度模块**：crate 数量冻结在当前规模；新增能力优先放进既有 crate 的子模块。

## 2. 目录与依赖规则

见 `AGENTS.md §1` 所有权地图。补充细则：

- 依赖只允许"下游引上游"：{protocol, sandbox-policy, sandbox-exec} ← {network-proxy, tools, session, context} ← agent-loop ← harness-cli
- `harness-cli` 是唯一含 `fn main` 的 crate
- crate 内部结构约定：
  ```
  crates/<name>/
  ├── Cargo.toml          # 依赖一律走 workspace 继承
  ├── src/lib.rs          # 模块门面：pub use 精确导出，不裸露内部类型
  └── src/<子模块>.rs      # 超过 ~400 行必须拆分
  ```
- 单文件超 400 行、单函数超 60 行视为需要拆分的信号

## 3. Rust 编码标准

### 3.1 命名与风格
- 遵循 rustfmt 默认（提交前 `cargo fmt --all`）
- 类型名 PascalCase；常量 SCREAMING_CASE；文档注释用 `//!`（模块级）与 `///`（条目级）
- 每个模块头部注释标注其对应设计原则编号（如 `P2`），保持代码↔原则可追溯

### 3.2 错误处理
| 层次 | 手段 |
|---|---|
| 协议边界 | `ErrorEnvelope { code: ErrorCode, message }` —— 可序列化、可路由 |
| 内部实现 | `std::io::Result` / 具体 Result；生产引入 thiserror 于 crate 边界 |
| 装配层（cli） | `anyhow::Result` 允许 |

禁止：`unwrap()` 出现在非测试代码（构造期不变量可用 `expect` 带说明）；解析 message 字符串做控制流。

### 3.3 并发纪律（预留）
- 主循环单活跃 turn 契约由显式断言保护（见 agent-loop）
- 生产切 tokio 时：工具执行走进程池，不在 loop 进程内跑外部命令

### 3.4 序列化
- 所有协议枚举：`#[serde(tag = "type", rename_all = "snake_case")]`
- 新增事件变体 = 向后兼容变更（允许）；删除/改义变体 = 破坏性变更（需 `contract/` 分支 + 人工 review + 版本号 minor bump）

## 4. 测试规范（P6）

1. **位置**：单元测试放各 crate `src/lib.rs` 底部 `#[cfg(test)] mod tests`；集成测试放 `crates/<name>/tests/`
2. **覆盖率**：新增公开函数必须有至少一个正例 + 一个反例/边界例
3. **故障注入**：凡依赖 `ModelProvider`/`Approver` 等 trait 的逻辑，测试必须覆盖失败路径（参考 `agent-loop::tests::model_failure_aborts_and_leaves_trace_never_silent`）
4. **不留痕迹的失败不是好失败**：断言异常路径也产生了对应 Event
5. 临时文件用 `std::env::temp_dir()` + 进程 id 组合命名，测后清理；不引入 tempfile 依赖除非任务卡批准

## 5. 协同工作流（人 + 多智能体）

### 5.1 角色分工
| 角色 | 职责 |
|---|---|
| 人类（架构守护者） | 下发任务卡、review `contract/*` 分支、裁决 CI 双绿但方案相悖的冲突 |
| 协调智能体（可选） | 拆解任务卡、分发、汇总各智能体汇报、检查红线自查表 |
| 执行智能体 | 按 AGENTS.md §3 循环完成单张任务卡 |

### 5.2 任务流转
```
任务卡下发 → 智能体认领声明 → 实现 → 自检(fmt/clippy/test)
→ 按 §6 模板汇报 → 人类或协调者验收 → 合并
```

### 5.3 冲突与裁决
- 一切以 `cargo test --workspace` 全绿为最低门槛
- 双方都绿但实现路线不同：人类依据本规范 §1 宪法裁决
- 修改/删除他人测试来通过自己实现 = 直接判负回滚

### 5.4 会话启动模板（给每个执行智能体的开场白）
```markdown
你正在 ideal-harness 仓库工作。先读 AGENTS.md 与 docs/DEVELOPMENT.md，
然后认领以下任务卡并严格在边界内工作：

### TASK-<n>: <目标>
- 目标 crate: <name>
- 验收标准: <可测试判据>
- 明确不做: <边界>

完成后按 AGENTS.md §6 模板汇报，附 cargo test 输出尾部。
```

## 6. 验收清单（DoD）

- [ ] `cargo build --workspace` 通过
- [ ] `cargo test --workspace` 全绿，且新增逻辑有测试覆盖
- [ ] `cargo clippy --workspace --all-targets` 无新警告
- [ ] `cargo fmt --all` 已执行
- [ ] 红线自查表逐项打勾（AGENTS.md §6 模板内嵌）
- [ ] 若触碰公开 API：已更新所有权地图注释与本文件的对应描述

## 7. 路线图（已迁移）

> **任务卡唯一来源已迁移至 `docs/ROADMAP.md`**——包含 P0~P5 六个阶段、
> 编号任务卡（含验收标准/明确不做/依赖关系）、质量门禁演进表与并行调度建议。
> 本节保留作为历史索引：

| 任务 | 目标 crate | 说明 | 状态 |
|---|---|---|---|
| TASK-101~104 | protocol/model-provider/agent-loop/cli | P1 可对话 MVP | 见 ROADMAP |
| TASK-201~205 | tools/sandbox-exec/network-proxy/approval | P2 安全纵深 | 见 ROADMAP |
| TASK-301~304 | context/session/agent-loop | P3 上下文工程 | 见 ROADMAP |
| TASK-401~412 | harness-cli/session/agent-loop/protocol | P4/P4.1 会话产品化与可靠性收口 | 见 ROADMAP |
| TASK-501~504 | tools/agent-loop/protocol/harness-cli | P5 扩展生态 | 见 ROADMAP |
