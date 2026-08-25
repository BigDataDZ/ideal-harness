# HANDOFF —— 开发交接文档

> 最后更新：TASK-102 完成后（commit `c880d0f`）
> 用途：任何人/智能体接手开发时，从本文档 5 分钟内恢复全部上下文。
> 阅读顺序：本文件 → `AGENTS.md` → `docs/ROADMAP.md`（任务卡）→ `docs/DESIGN-DECISIONS.md`（架构依据）→ `docs/DEVELOPMENT.md`（完整规范）

---

## 一、项目一句话与位置

- **是什么**：Rust 实现的 LLM Agent Harness 原型（protocol-first / 事件溯源 / 三层沙箱 / fail-closed 审批）
- **仓库**：`D:\ds\ideal-harness`（git 已初始化，main 分支）
- **设计来源**：对 OpenAI Codex CLI（`D:\harness\codex`）与 DeepSeek Harness（`D:\harness\DeepSeek-Harness`）的源码级对比研究，实证报告在 `D:\ds\harness-comparison-report.md`，设计原则在 `D:\ds\ideal-harness-design.md`

## 二、当前状态总览

| 阶段 | 状态 | 证据 |
|---|---|---|
| P0 架构骨架（v0.1.0） | ✅ 完成 | 8 crate / 32 测试全绿；commit `03a5255` |
| TASK-101 协议流式契约 | ✅ 完成 | commit `b661d4f`；protocol 5 测试（含旧 JSONL 兼容重放） |
| **TASK-102 model-provider** | ✅ 完成 | commit `c880d0f`；model-provider 10 测试（5 单元 + 5 故障注入），workspace 合计 42 全绿 |
| TASK-103 / 104 | ⬜ 未开始 | 依赖 101（已满足）/ 102 |
| P2~P5 | ⬜ 未开始 | 见 ROADMAP |

## 三、已完成工作的关键事实

1. **测试基线**：`cargo test --offline --workspace` = **32 passed, 0 failed**
2. **已知遗留**：`agent-loop` 测试助手有 1 条 clippy 警告（`&PathBuf`→`&Path`，P0 遗留）——TASK-103 同 crate，届时顺手修；P1 起 clippy 升 `-D warnings` 硬门禁
3. **协议现状**（TASK-101 之后）：`Event` 含 10 个变体（新增 `ModelChunkReceived { call_id, delta_text }`）；新增 `ModelCallSpec { model, base_url, temperature: Option<f32> }`（无认证字段——认证属 provider 层，这是 TASK-102 的输入）
4. **git 提交链**：`03a5255`（骨架+规范）→ `b661d4f`（TASK-101）。git 身份是占位符 `dev@ideal-harness.local`，**推送 GitHub 前需改**：`git config user.name/user.email`

## 四、🔄 TASK-102 断点详情（接手人从这里继续）

### 已勘察确认的事实（不用重查）

- **本机依赖缓存**（`~/.cargo/registry/cache`）已确认存在：`reqwest 0.12.28` 与 `0.13.4`、`tokio 1.52.3`、`hyper 1.10.1`、rustls 全家（`rustls 0.23.40` / `webpki` / `tokio-rustls` / `hyper-rustls` / `rustls-platform-verifier`）、**`ring 0.17.14`**（rustls 加密后端 ✓）、`bytes/futures/url/idna/mime/encoding_rs/ipnet/serde_urlencoded/sync_wrapper/tower` 等全部传递依赖
- **native-tls/schannel 不在缓存** → 必须用 `default-features = false, features = ["blocking", "json", "rustls-tls"]`，**不要用默认 TLS**（离线会失败）
- crates.io 网络被沙箱拦截 → 全程 `cargo build/test --offline`

### 已定的设计（按 DESIGN-DECISIONS D4/D12 推导，接手人可直接采用）

1. **同步阻塞式**（`reqwest::blocking`）——当前 agent-loop 是同步骨架，不引入 tokio 到公开接口
2. **纯解析层与 IO 分离**（可独立测试）：
   - `parse_sse_line(&str) -> SseLine`（Ignore / Done / Data(String)）；`data:` 前缀、`[DONE]` 哨兵、其余忽略
   - `extract_delta(&str) -> Result<(Option<String>, Option<String>), ErrorEnvelope>`：结构化匹配 `choices[0].delta.content` 与 `finish_reason` 字段；**非 JSON 行 → `ModelStreamBroken`**（不静默跳过）
3. **错误映射**（结构化字段匹配，非 message 解析——红线）：`error.code == "context_length_exceeded"` → `ContextWindowExceeded`；其余非 2xx → `Internal`；流中断/超时/截断 → `ModelStreamBroken`
4. **截断判定**：流结束但未见 `[DONE]` → `ModelStreamBroken`（"流在哨兵前结束"）
5. **认证**：`from_env()` 读 `IDEAL_HARNESS_API_KEY`，缺失 → `Internal`；另有 `with_key()` / `with_key_and_timeout()`（测试用短超时）
6. **trait 边界**：只留一个 `trait ChatModel { fn stream_chat(&self, &ModelCallSpec, &[ChatMessage]) -> Result<ChatReply, ErrorEnvelope> }`——任务卡「明确不做」多 provider 抽象层
7. **验收测试**（任务卡要求的三种故障注入）：本地 `std::net::TcpListener` mock server 手写 HTTP 响应（`Content-Length` 精确计算），① 超时（handler 挂起不响应 + 300ms 客户端超时）② 截断（发半行即断，无 `[DONE]`）③ 非 JSON data 行；⚠️ 本机沙箱是否放行 localhost socket **未验证**——若被拦，给测试加 `#[ignore = "sandbox blocks local sockets"]` 并在汇报中如实记录，纯解析层测试仍可覆盖 ②③

### 剩余步骤清单（按序执行）

1. 新建 `crates/model-provider/{Cargo.toml, src/lib.rs, tests/fault_injection.rs}`（依赖：protocol/serde/serde_json/reqwest 上述 features）
2. 根 `Cargo.toml` 的 `[workspace.dependencies]` 加 `reqwest`（版本 `"0.12"`）与 `model-provider` path
3. `AGENTS.md` §1 所有权地图加行：`crates/model-provider | OpenAI 兼容 HTTP+SSE 客户端 | P1 | protocol, reqwest`（任务卡授权）
4. `CHANGELOG.md` Unreleased/Added 加行；`docs/ROADMAP.md` TASK-102 标 ✅ 附 commit
5. `cargo fmt --all` → `cargo clippy --offline --workspace --all-targets` → `cargo test --offline --workspace` 全绿
6. 提交：`feat(model-provider): TASK-102 OpenAI-compatible SSE client`；按 AGENTS.md §6 模板汇报

## 五、环境注意点（踩过的坑，务必看）

1. **离线构建**：一律 `--offline`；新增依赖前查缓存（本文件第四节已替你查好 TASK-102 的）
2. **PowerShell stderr 误报**：`cargo ... 2>&1` 会让 harness 显示 `[exit code: 1]` 即使成功——**以输出中的 `Finished` / `test result: ok` 行为准**
3. **edit 工具先读后改**：对未在本会话读过的文件直接 edit 会被拒——先 `read` 再 `edit`
4. **并行写入竞态**：一次消息里并行多个 `write` 偶发丢文件（P0 时根 Cargo.toml 丢过一次）——关键文件写完用 `glob`/`Get-ChildItem` 复核
5. **模型空响应**：harness 偶发"completed response with no content"失败——瞬时故障，重发即可，不是项目问题

## 六、常用命令速查

```bash
cd D:\ds\ideal-harness
cargo build --offline --workspace        # 编译
cargo test --offline --workspace         # 全量测试（合并门槛）
cargo clippy --offline --workspace --all-targets
cargo run -p harness-cli                 # 最小演示（沙箱/工具自纠/事件回放）
cargo build --offline --release -p harness-cli   # 449KB 单文件产物
git log --oneline                        # 当前 2 个提交
```

## 七、任务认领状态板

| 任务 | 状态 | 认领人 |
|---|---|---|
| TASK-101 | ✅ 完成（b661d4f） | ox-alpha（本会话） |
| TASK-102 | ✅ 完成（c880d0f） | ox-alpha（本会话） |
| TASK-103/104 | ⬜ 可认领 | — |
| TASK-201~205 | ⬜（P2，需先完成 P1） | — |
