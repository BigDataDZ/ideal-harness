import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

type ShellState = "connecting" | "ready" | "unavailable";

function App() {
  const [shellState, setShellState] = useState<ShellState>("connecting");
  const [statusText, setStatusText] = useState("正在连接受限 Rust 宿主…");

  useEffect(() => {
    let active = true;

    invoke<string>("desktop_status")
      .then((status) => {
        if (active) {
          setStatusText(status);
          setShellState("ready");
        }
      })
      .catch(() => {
        if (active) {
          setStatusText("请通过 Tauri 桌面入口启动；浏览器预览不连接本地宿主");
          setShellState("unavailable");
        }
      });

    return () => {
      active = false;
    };
  }, []);

  return (
    <main className="shell">
      <section className="hero" aria-labelledby="app-title">
        <div className="brand-mark" aria-hidden="true">
          IH
        </div>
        <p className="eyebrow">IDEAL HARNESS · DESKTOP</p>
        <h1 id="app-title">可靠的 Agent，清晰的边界。</h1>
        <p className="summary">
          Tauri 只承载界面，Rust Harness 继续管理会话、工具、沙箱与审批。所有状态最终都能由事件流重建。
        </p>

        <div className={`status status--${shellState}`} role="status" aria-live="polite">
          <span className="status__dot" aria-hidden="true" />
          <span>{statusText}</span>
        </div>
      </section>

      <section className="principles" aria-label="桌面端安全原则">
        <article>
          <span>01</span>
          <h2>唯一真相源</h2>
          <p>客户端只投影 Event，不在 WebView 内复制业务状态。</p>
        </article>
        <article>
          <span>02</span>
          <h2>默认无权限</h2>
          <p>Capability 从空白名单起步，能力按任务逐项开放。</p>
        </article>
        <article>
          <span>03</span>
          <h2>显式桥接</h2>
          <p>敏感操作只通过经过校验的 Rust command 进入核心。</p>
        </article>
      </section>

      <footer>
        <span>TASK-901</span>
        <span>桌面骨架已就绪 · 业务装配将在 TASK-902 接入</span>
      </footer>
    </main>
  );
}

export default App;
