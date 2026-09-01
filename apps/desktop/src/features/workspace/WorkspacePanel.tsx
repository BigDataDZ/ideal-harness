import { useMemo, useState } from "react";

import type { ProjectionSnapshot } from "../../lib/projection/index.ts";
import { buildLineDiff } from "./diff.ts";
import { projectWorkspace } from "./projection.ts";
import type { FileChange } from "./types.ts";

export function WorkspacePanel({ snapshot }: { snapshot: ProjectionSnapshot | null }) {
  const view = useMemo(() => projectWorkspace(snapshot), [snapshot]);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const selected = view.files.find((file) => file.path === selectedPath) ?? view.files[0] ?? null;

  return (
    <section className="workspace-feature" aria-labelledby="workspace-title">
      <header className="feature-header">
        <div><p className="panel-kicker">READ-ONLY PROJECTION</p><h2 id="workspace-title">工作区观察</h2></div>
        <span className="read-only-badge">只读 · 无保存入口</span>
      </header>
      <p className="workspace-notice">文件列表、预览和变更只来自已审计的 fs_* Event；路径约束和 symlink 防逃逸由 Rust 工具层执行。</p>
      {view.integrityIssues.length > 0 ? (
        <div className="workspace-issues" role="alert"><strong>已拒绝不安全的投影数据</strong>{view.integrityIssues.map((issue) => <code key={issue}>{issue}</code>)}</div>
      ) : null}

      <div className="workspace-browser">
        <aside className="file-tree" aria-label="已观察文件">
          <h3>文件</h3>
          {view.files.length === 0 ? <p className="muted">事件流中还没有安全的文件观察记录。</p> : (
            <ul>{view.files.map((file) => <li key={file.path}><button type="button" aria-current={selected?.path === file.path ? "page" : undefined} onClick={() => setSelectedPath(file.path)}><span aria-hidden="true">◇</span>{file.path}</button></li>)}</ul>
          )}
        </aside>
        <section className="file-preview" aria-labelledby="file-preview-title">
          <header><h3 id="file-preview-title">{selected?.path ?? "文件预览"}</h3>{selected ? <span>Event #{selected.sourceSeq}</span> : null}</header>
          {!selected ? <p className="muted">选择一个已观察文件。</p> : selected.content === null ? <p className="muted">只有路径记录；需由 fs_read 事件提供内容后才能预览。</p> : (
            <><pre>{selected.content}</pre>{selected.hash ? <p>hash <code>{selected.hash}</code></p> : <p>当前投影未提供 hash</p>}{selected.truncated ? <p className="warning-text">内容为截断预览，完整内容未加载到 WebView。</p> : null}</>
          )}
        </section>
      </div>

      <section className="change-list" aria-labelledby="change-list-title">
        <h3 id="change-list-title">安全 Diff</h3>
        {view.changes.length === 0 ? <p className="muted">事件流中没有 fs_write/fs_edit 变更。</p> : view.changes.map((change) => <ChangeCard key={change.callId} change={change} />)}
      </section>
    </section>
  );
}

function ChangeCard({ change }: { change: FileChange }) {
  const lines = buildLineDiff(change.before, change.after);
  const diffTruncated = (change.before?.split("\n").length ?? 0) > 400 || (change.after?.split("\n").length ?? 0) > 400;
  return (
    <article className={`change-card change-card--${change.status}`}>
      <header><div><span>{change.tool}</span><strong>{change.path}</strong></div><span>{change.status}</span></header>
      <dl className="change-facts">
        <div><dt>调用</dt><dd><code>{change.callId}</code></dd></div>
        <div><dt>Event</dt><dd>#{change.requestSeq}{change.resultSeq === null ? " → pending" : ` → #${change.resultSeq}`}</dd></div>
        <div><dt>expected_hash</dt><dd><code>{change.expectedHash ?? "缺失"}</code></dd></div>
        <div><dt>CAS 观察</dt><dd className={change.casVerified ? "positive-text" : "warning-text"}>{change.casVerified ? "与最近 fs_read hash 一致" : "无法由事件投影验证"}</dd></div>
      </dl>
      {change.errorCode ? <p className="change-error" role="alert">{change.errorCode}</p> : null}
      {lines.length === 0 ? <p className="muted">缺少可配对的读前内容，无法安全生成文本 Diff。</p> : (
        <div className="diff-view" role="table" aria-label={`${change.path} Diff`}>
          {lines.map((line, index) => <div className={`diff-line diff-line--${line.kind}`} role="row" key={`${index}-${line.kind}`}><span>{line.beforeLine ?? ""}</span><span>{line.afterLine ?? ""}</span><code>{line.kind === "added" ? "+" : line.kind === "removed" ? "−" : " "} {line.text}</code></div>)}
        </div>
      )}
      {diffTruncated ? <p className="warning-text">Diff 仅展示前 400 行；请按 Event 定位完整变更。</p> : null}
    </article>
  );
}
