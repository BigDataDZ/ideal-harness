<!-- PR 标题格式：<type>(<crate>): <描述>，如 feat(tools): 完整 schema 校验器 -->

## 任务卡

<!-- 关联 ROADMAP 任务编号，如 TASK-103；协议变更需 contract/* 分支 -->

TASK-___：

## 影响模块清单

<!-- 对照 AGENTS.md §1 所有权地图逐个列出；触及 D1~D13 决策须引用决策编号 -->

- [ ] crates/

## 自检结果（DoD）

- [ ] `cargo build --workspace` 通过
- [ ] `cargo test --workspace` 全绿（新增公开函数含正例+失败路径用例）
- [ ] `cargo clippy --workspace --all-targets` 无新警告
- [ ] `cargo fmt --all` 已执行

## 测试证据

<!-- 粘贴 cargo test 输出尾部 -->

```
test result: ...
```

## 红线自查（AGENTS.md §2）

- [ ] 未触碰 protocol（或任务卡明确要求且已同步消费方与序列化测试）
- [ ] 错误仅按 ErrorCode 路由，未解析 message 字符串
- [ ] fail-closed 分支全部走向拒绝
- [ ] 未拆散 tool_call/tool_result 配对
- [ ] 自动行为均落 Event
- [ ] 未删除/跳过他人测试
- [ ] 未引入任务卡之外的新依赖

## 遗留与建议

<!-- 相邻问题记在这里，不越界修 -->
