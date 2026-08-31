import type { ProjectionSnapshot } from "../../lib/projection/index.ts";
import { buildTimelineView } from "./diagnostics.ts";

export function TimelinePanel({ snapshot }: { snapshot: ProjectionSnapshot | null }) {
  if (!snapshot) {
    return (
      <section className="timeline-shell timeline-shell--empty" aria-labelledby="timeline-title">
        <p className="panel-kicker">EVENT PROJECTION</p>
        <h2 id="timeline-title">选择一个会话</h2>
        <p>Timeline、错误码、Token 和 Agent Team 状态只会从权威事件流生成。</p>
      </section>
    );
  }

  const view = buildTimelineView(snapshot);
  return (
    <section className="timeline-shell" aria-labelledby="timeline-title">
      <header className="timeline-header">
        <div>
          <p className="panel-kicker">EVENT PROJECTION</p>
          <h2 id="timeline-title">{view.sessionId}</h2>
        </div>
        <ConnectionBadge connection={view.connection} generation={view.generation} />
      </header>

      {view.repairReason ? (
        <div className="inline-alert" role="alert">
          事件投影需要修复：{view.repairReason}。修复完成前不显示推测状态。
        </div>
      ) : null}

      <div className="diagnostic-grid" aria-label="会话诊断摘要">
        <Metric label="Turn" value={`${view.completedTurns} 完成`} detail={`${view.activeTurns} 活跃 · ${view.abortedTurns} 中止`} />
        <Metric
          label="Token"
          value={view.tokenUsage.toLocaleString()}
          detail={view.tokenRemaining === null ? "未配置预算" : `剩余 ${view.tokenRemaining.toLocaleString()}`}
        />
        <Metric label="Agent Team" value={`${view.teamMembers.length} members`} detail={`${view.teamTasks.length} tasks · ${view.teamConflicts} 冲突`} />
        <Metric label="Watermark" value={view.lastEventId ?? "—"} detail={`generation ${view.generation ?? "—"}`} />
      </div>

      <div className="timeline-layout">
        <div className="event-column">
          <div className="section-heading">
            <h3>事件时间线</h3>
            <span>{view.rows.length}</span>
          </div>
          {view.rows.length === 0 ? (
            <p className="muted">这个会话还没有事件。</p>
          ) : (
            <ol className="event-list" aria-label="事件时间线">
              {view.rows.map((row) => (
                <li key={row.seq} className={`event-row event-row--${row.tone}`}>
                  <span className="event-seq">#{row.seq}</span>
                  <span className="event-marker" aria-hidden="true" />
                  <div>
                    <strong>{row.label}</strong>
                    <p>{row.summary}</p>
                    <code>{row.type}</code>
                  </div>
                </li>
              ))}
            </ol>
          )}
        </div>

        <aside className="diagnostic-column" aria-label="详细诊断">
          <DiagnosticSection title="稳定错误码" count={view.errors.length}>
            {view.errors.length === 0 ? <p className="muted">没有结构化错误。</p> : view.errors.map((error) => (
              <article key={`${error.seq}-${error.code}`} className="error-card">
                <code>{error.code}</code>
                <strong>{error.title}</strong>
                <p>{error.message}</p>
              </article>
            ))}
          </DiagnosticSection>
          <DiagnosticSection title="Agent Team" count={view.teamTasks.length}>
            {view.teamTasks.length === 0 ? <p className="muted">没有 Team 任务事件。</p> : view.teamTasks.map((task) => (
              <article key={task.taskId} className="team-task">
                <div><strong>{task.taskId}</strong><span>{task.status}</span></div>
                <p>{task.owner} · revision {task.revision}</p>
                {task.blockedBy.length > 0 ? <p>blocked by {task.blockedBy.join(", ")}</p> : null}
              </article>
            ))}
            <p className="diagnostic-note">待送达消息 {view.teamMessagesPending} · 写范围冲突 {view.teamConflicts}</p>
          </DiagnosticSection>
          <DiagnosticSection title="Token 来源" count={null}>
            <dl className="token-breakdown">
              <div><dt>Provider</dt><dd>{view.tokenSources.provider}</dd></div>
              <div><dt>Heuristic</dt><dd>{view.tokenSources.heuristic}</dd></div>
            </dl>
          </DiagnosticSection>
        </aside>
      </div>
    </section>
  );
}

function ConnectionBadge({
  connection,
  generation,
}: {
  connection: ProjectionSnapshot["connection"];
  generation: number | null;
}) {
  const label = connection === "connected" ? "已连接" : connection === "disconnected" ? "已断线" : "未连接";
  return (
    <span className={`connection-badge connection-badge--${connection}`} role="status">
      <i aria-hidden="true" /> {label} · gen {generation ?? "—"}
    </span>
  );
}

function Metric({ label, value, detail }: { label: string; value: string; detail: string }) {
  return <article className="metric"><span>{label}</span><strong>{value}</strong><p>{detail}</p></article>;
}

function DiagnosticSection({
  title,
  count,
  children,
}: {
  title: string;
  count: number | null;
  children: React.ReactNode;
}) {
  return (
    <section className="diagnostic-section">
      <div className="section-heading"><h3>{title}</h3>{count !== null ? <span>{count}</span> : null}</div>
      {children}
    </section>
  );
}
