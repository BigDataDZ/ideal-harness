# AGENTS.md —— ideal-harness 协同开发规范（智能体与人共用入口）

> 本文件是所有 AI 智能体与人类贡献者进入仓库的**第一读物**。
> 完整规范见 `docs/DEVELOPMENT.md`；两者冲突时以 DEVELOPMENT.md 为准。

## 0. 项目一句话

Rust 实现的 LLM Agent Harness 原型：protocol-first、事件溯源、三层沙箱、fail-closed 审批。

**设计依据链**：每个架构决策的对标来源（学的谁/不同于谁/踩什么坑）见 `docs/DESIGN-DECISIONS.md`（D1~D13 决策对照表 + 环境注意点）。改架构前必读；PR 触及表中决策时必须在描述里引用决策编号。

## 1. 模块所有权地图（改动边界）

| crate | 职责 | 设计原则 | 允许依赖 |
|---|---|---|---|
| `crates/protocol` | wire 协议：Event/ErrorCode/ErrorEnvelope，**唯一契约** | P-arch | serde |
| `crates/sandbox-policy` | SandboxMode 单一抽象 + 词法栅栏 + 加宽表 | P2 | — |
| `crates/approval` | fail-closed 审批 + 提权参数成对校验 | P2 | protocol, sandbox-policy |
| `crates/tools` | 工具注册表 + schema 校验 + 调度 | P3/P4 | protocol |
| `crates/session` | JSONL 事件溯源：append/replay/fork | P5 | protocol |
| `crates/context` | token 预算 + 双触发压缩判定 | P4 | protocol |
| `crates/model-provider` | OpenAI 兼容 HTTP+SSE 客户端（错误→稳定码映射） | P1 | protocol, reqwest |
| `crates/agent-loop` | Phase 状态机主循环 + Inbox + 工具调用闭环 | P3 | protocol, session, tools, model-provider |
| `crates/harness-cli` | 装配入口（唯一允许 main 的地方） | — | 全部 |

依赖方向必须与上表一致，禁止反向依赖与跨层依赖。

## 2. 红线（违反 = PR 必拒）

1. **协议冻结**：不改 `crates/protocol` 除非任务卡明确要求；改契约必须同步更新全部消费方与序列化测试
2. **错误纪律**：控制流只匹配 `ErrorCode`，**禁止解析 message 字符串**
3. **fail-closed**：任何审批/沙箱路径的"服务不在场"分支必须走向拒绝
4. **配对完整**：任何裁剪不得拆散 tool_call/tool_result
5. **留痕**：一切自动行为（压缩/重试/审批）必须落 Event
6. 不删除或跳过他人测试；新增公开函数必须带测试
7. 不引入新的 workspace 外部依赖，除非任务卡明确列出

## 3. 智能体工作循环（每个会话照此执行）

```
读 AGENTS.md → 选一张任务卡 → 声明"我认领 <crate>/<功能>"
→ 只在认领范围内改代码 → cargo fmt → cargo clippy --workspace
→ cargo test --workspace 全绿 → 按 §6 模板汇报
```

- 一次会话只做一张任务卡的事；发现相邻问题→记入汇报"遗留"，不越界修
- 无法完成时：如实说明卡点+已验证的部分，禁止假装完成

## 4. 任务卡格式（人类或协调者下发）

```markdown
### TASK-<n>: <一句话目标>
- 目标 crate: tools
- 验收标准: (可测试的判据 1~3 条)
- 明确不做: (边界)
```

## 5. git 与提交约定

- 分支：`feature/<crate>-<短描述>`；协议变更：`contract/<desc>`（需人工 review）
- 提交信息：Conventional Commits（`feat(tools): ...` / `fix(session): ...`）
- PR 描述必填：影响模块清单 + 测试证据（粘贴 cargo test 输出尾部）

## 6. 完成汇报模板（智能体输出）

```markdown
## 任务汇报: TASK-<n>
- 认领模块: <crate>
- 变更文件: <列表>
- 自检结果: build ✅ / test N passed ✅ / clippy 无新警告 ✅
- 红线自查: 未触碰 protocol ✅ / 错误仅按 code 路由 ✅ / ...
- 遗留与建议: ...
```

## 7. 快速命令

```bash
cargo build --workspace          # 编译
cargo test --workspace           # 全部测试（合并前必须全绿）
cargo clippy --workspace --all-targets  # lint
cargo run -p harness-cli         # 跑最小演示
cargo fmt --all                  # 格式化
```

## 8. 冲突解决

两个智能体产出冲突时：**以 CI 为准，以测试为裁决**。都绿则由人类按设计原则裁决；
任何一方为了让自己的实现通过而修改/删除对方测试，直接判负。
