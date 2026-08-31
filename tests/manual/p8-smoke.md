# P8 真实模型端到端冒烟规程（TASK-808 验收 2）

> 目的：验证生产 CLI（非测试装配）在真实模型下完成一次仓库代码任务。
> 本记录必须由持有 `IDEAL_HARNESS_API_KEY` 的人执行一次并回填结果；CI 不使用真实 key。

## 当前状态

- 状态：待执行（不是失败，也未以 scripted-provider 结果冒充真实模型通过）。
- 最近检查：2026-08-31，当前执行环境未设置 `IDEAL_HARNESS_API_KEY`。
- 已有基线：scripted-provider 生产装配端到端测试和远程四项 CI 门禁均通过；CI 证据见
  [GitHub Actions run #13](https://github.com/BigDataDZ/ideal-harness/actions/runs/33353637643)。
- 解锁条件：仅需为执行进程提供 `IDEAL_HARNESS_API_KEY`，无需修改代码或把 key 写入仓库。

## 前置

- `IDEAL_HARNESS_API_KEY` 已设置（DeepSeek 兼容端点）。
- 在一个**可丢弃的试验仓库**中执行（本规程不自动修改真实用户仓库）。
- 工具清单应包含：fs_read/fs_write/fs_edit/fs_glob/fs_grep/web_fetch/memory_write/exec。

## 步骤

```bash
cd <试验仓库>
ideal-harness chat --workspace . --session .\smoke-p8.jsonl --fetch-allow <可选域名>
```

1. 输入任务：`读取 src/，找出 add 函数并把它改为 a + 2，然后运行 cargo test 验证。`
2. 观察模型依次调用 fs_grep → fs_read → fs_edit（携带 expected_hash）→ exec。
3. 编辑被外部篡改时应看到 FileRevisionConflict（可用第二个终端修改文件复现）。
4. exec 提权时应出现终端 y/n 审批；拒绝后工具结果为稳定失败码。
5. 任务中途用 `/steer 顺便把注释更新一下` 验证 steer 在下一采样边界生效。
6. 完成后 `/exit`，再 `ideal-harness chat --session .\smoke-p8.jsonl --workspace .` 验证 resume。

## 回填记录（执行后填写）

- 日期 / 模型：
- 命令与参数：
- 工具调用序列：
- 遇到的失败与恢复：
- 最终 diff 与测试结果：
- 备注（超时 / 循环防护 / spill 是否出现）：
