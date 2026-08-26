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

## 2. ✅ 真实 key 多轮对话 + 工具调用（已验证 2026-08-25，OpenRouter + minimax/minimax-m3:free）

```powershell
$env:IDEAL_HARNESS_API_KEY = "sk-or-..."   # OpenRouter key（不写入仓库）
.\target\release\ideal-harness.exe chat --session $env:TEMP\mvp-smoke.jsonl `
    --base-url https://openrouter.ai/api/v1 --model minimax/minimax-m3:free
```

实际终端记录：

```
> 请调用 now 工具查询当前时间
  ⚙ 调用 now({})
assistant: 当前 Unix 时间戳为：1787745074
对应的日期时间大约为 2026 年 5 月 25 日…
> 请调用 echo 工具把 hello 原样返回
  ⚙ 调用 echo({"text":"hello"})
assistant: 已原样返回：hello
> 我上一条让你把什么原样返回？
assistant: 您上一条让我把 "hello" 原样返回。
> /exit
会话已保存：C:\Users\<uid>\AppData\Local\Temp\mvp-smoke7.jsonl
```

三项出口判据全部命中：① 工具调用闭环（now/echo 各一次，调用→执行→回填→二次采样）
② 工具结果进入模型上下文 ③ 跨轮记忆（第三问引用首轮结果）。

**冒烟过程中发现并修复一个真实协议 bug**：assistant 工具调用消息的序列化形状
（扁平 → OpenAI 嵌套 `type:"function"` 形状），由
`assistant_tool_calls_serialize_to_nested_wire_shape` 测试锁定。

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
