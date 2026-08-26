# chat 子命令手动冒烟脚本（TASK-104 验收记录）

> 产物：`target/release/ideal-harness.exe`（`cargo build --offline --release -p harness-cli`）
> 本文档 = TASK-104 验收判据「手动冒烟脚本记录」。已验证项带 ✅ 与实际输出。

## 0. 构建

```powershell
cd D:\ds\ideal-harness
cargo build --offline --release -p harness-cli
```

## 1. ✅ 无 key fail-closed（已验证 2026-08-25）

不设置 `IDEAL_HARNESS_API_KEY` 直接启动，必须拒绝且不发起任何网络请求（红线 3）：

```
PS> .\target\release\ideal-harness.exe chat
Error: 环境变量 IDEAL_HARNESS_API_KEY 未设置；拒绝以匿名方式调用上游
       （请先设置环境变量 IDEAL_HARNESS_API_KEY 后重试）
（退出码 1）
```

## 2. 真实 key 多轮对话 + 工具调用（MVP 出口判据，待执行）

```powershell
$env:IDEAL_HARNESS_API_KEY = "sk-..."   # DeepSeek 或任意 OpenAI 兼容端点
.\target\release\ideal-harness.exe chat --session D:\tmp\chat1.jsonl
```

对话脚本与期望：

| 输入 | 期望 |
|---|---|
| `现在几点了？` | 打印 `⚙ 调用 now(...)` 后给出当前时间（工具闭环） |
| `把 "hello" 原样返回给我` | 打印 `⚙ 调用 echo(...)`，答复含 hello |
| `我刚才让你返回什么？` | 答复引用 "hello"——**多轮记忆生效** |
| `/exit` | 退出并打印会话保存路径 |

执行后把完整终端记录粘贴到本节（含时间戳与事件投影行）。

## 3. ✅ 会话复用与中断恢复（已验证，单测锁定）

- `--session <path>` 指向既有 JSONL：seq 自动续接，历史由事件流重建（`rebuild_history_pairs_user_and_assistant_only`）
- 上次 Ctrl+C 硬退出留下的悬空 turn：下次启动自动补记 `TurnAborted`（`dangling_turn_recovered_on_reopen_then_noop`）
- 已正常收口的会话重开不产生任何补写（`finished_turn_not_recovered`）

## 4. ✅ demo 模式回归（已验证 2026-08-25）

无参数运行 `ideal-harness` 保留 v0.1 演示：沙箱语义 / 工具缺参自纠 / 事件流回放，输出正常。

## 5. 已知边界（记入 P2+）

- Ctrl+C 为进程级硬退出（同步阻塞读行的 std 限制）；会话文件因 append-only 天然不损坏，
  悬空 turn 由下次启动的事件溯源恢复收口——语义等价"优雅中止留 TurnAborted"
- 工具调用中间态不进模型历史，由该 turn 最终 assistant 文本概括（P3 压缩工程接管）
