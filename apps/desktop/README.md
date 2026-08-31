# ideal-harness desktop

P9/TASK-901 的受限桌面骨架，采用 Tauri 2 + React + TypeScript + Vite。

## 开发

```powershell
npm install
npm run typecheck
npm run build
npm run tauri dev
```

单独验证 Rust 入口：

```powershell
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## 安全边界

- WebView 只负责投影，不持有业务真相源。
- CSP 禁止远程脚本、对象和 frame。
- `desktop-shell` capability 从空权限列表开始。
- TASK-901 仅暴露无敏感信息的 `desktop_status`；业务 command 从 TASK-903 起逐项审计。
- 当前入口不依赖核心 crate、不复制 agent-loop，也不开放 shell 或通用文件系统 API。
