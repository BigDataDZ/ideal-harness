# Contributing to ideal-harness

感谢关注！本项目同时面向人类贡献者与 AI 智能体协作，规则统一。

## 三步上手

1. **读规范**：[AGENTS.md](AGENTS.md) 是入口（含红线清单），细节在
   [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)，架构决策依据在
   [docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md)。
2. **领任务**：从 [docs/ROADMAP.md](docs/ROADMAP.md) 选一张未被认领的任务卡；
   涉及 `crates/protocol` 的改动走 `contract/*` 分支并需维护者 review。
3. **交作业**：按 Conventional Commits 提交，PR 描述必填影响模块清单 +
   测试证据（粘贴 `cargo test` 输出尾部）。

## 合并最低门槛（DoD）

- `cargo build --workspace` 通过
- `cargo test --workspace` 全绿，新增公开函数带测试（含失败路径）
- `cargo clippy --workspace --all-targets` 无新警告
- `cargo fmt --all` 已执行
- 红线自查表逐项打勾（AGENTS.md §6 模板）

## 铁律速览（完整版见 AGENTS.md §2）

- 不改协议除非任务卡明确要求
- 错误只按 `ErrorCode` 路由，禁止解析 message 字符串
- fail-closed：审批/沙箱"服务不在场"必须拒绝
- 裁剪不拆 tool_call/result 配对；自动行为必须落 Event
- 修改/删除他人测试来让自己的实现通过 = PR 直接关闭

## 报告问题

- 功能与缺陷：GitHub Issues，附最小复现
- **安全漏洞：勿公开**，走 [SECURITY.md](SECURITY.md) 私密通道
