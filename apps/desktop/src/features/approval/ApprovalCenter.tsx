import { useMemo, useState } from "react";

import type { ProjectionSnapshot } from "../../lib/projection/index.ts";
import { projectApprovalCenter } from "./projection.ts";
import type { CurrentSecurityFacts, PendingApprovalRequest } from "./types.ts";

interface ApprovalCenterProps {
  snapshot: ProjectionSnapshot | null;
  request: PendingApprovalRequest | null;
  security: CurrentSecurityFacts | null;
  onDecision: (request: PendingApprovalRequest, approved: boolean) => Promise<void>;
}

export function ApprovalCenter({ snapshot, request, security, onDecision }: ApprovalCenterProps) {
  const view = useMemo(() => projectApprovalCenter(snapshot, request, security), [snapshot, request, security]);
  const [reviewedRequestId, setReviewedRequestId] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const actionable = request !== null && view.readiness.kind === "ready" && !submitting;
  const reviewed = request !== null && reviewedRequestId === request.requestId;

  const decide = (approved: boolean) => {
    if (!request || view.readiness.kind !== "ready" || (approved && !reviewed)) return;
    setSubmitting(true);
    setFailure(null);
    void onDecision(request, approved)
      .catch((cause: unknown) => setFailure(commandFailure(cause)))
      .finally(() => setSubmitting(false));
  };

  return (
    <section className="approval-shell" aria-labelledby="approval-title">
      <header className="feature-header">
        <div><p className="panel-kicker">SECURITY GATE</p><h2 id="approval-title">审批中心</h2></div>
        <span className={`approval-readiness approval-readiness--${view.readiness.kind}`}>{readinessLabel(view.readiness.kind)}</span>
      </header>

      {!request ? (
        <div className="feature-empty" role="status"><strong>没有可操作的审批</strong><p>审批服务或窗口不在场时，宿主会默认拒绝，不会静默放行。</p></div>
      ) : (
        <article className="approval-request">
          <div className="approval-risk"><span>{request.sandboxMode}</span><strong>{request.justification}</strong></div>
          <dl className="security-facts">
            <Fact label="实际程序" value={request.command} code />
            <Fact label="完整参数" value={JSON.stringify(request.arguments)} code />
            <Fact label="工作区" value={request.workspace} code />
            <Fact label="权限 epoch" value={String(request.permissionEpoch)} />
            <Fact label="权限配置" value={request.permissionProfileHash} code />
            <Fact label="执行环境" value={`${request.executor.os} · generation ${request.executor.generation}`} />
            <Fact label="执行 Home" value={request.executor.home} code />
            <Fact label="call id" value={request.callId} code />
          </dl>
          {view.readiness.kind !== "ready" ? (
            <div className="approval-blocked" role="alert"><strong>此请求不可批准</strong>{view.readiness.issues.map((issue) => <span key={issue}>{issue}</span>)}</div>
          ) : null}
          <label className="approval-review"><input type="checkbox" checked={reviewed} onChange={(event) => setReviewedRequestId(event.currentTarget.checked ? request.requestId : null)} disabled={!actionable} />我已核对上述实际参数与安全事实</label>
          <div className="approval-actions">
            <button type="button" className="danger-button" disabled={!actionable} onClick={() => decide(false)}>拒绝本次</button>
            <button type="button" disabled={!actionable || !reviewed} onClick={() => decide(true)}>仅批准本次</button>
          </div>
          {failure ? <p className="approval-failure" role="alert">审批提交失败：{failure}</p> : null}
        </article>
      )}

      <section className="approval-history" aria-labelledby="approval-history-title">
        <h3 id="approval-history-title">审批审计</h3>
        {view.history.length === 0 ? <p className="muted">当前事件流没有审批记录。</p> : (
          <ol>{view.history.map((row) => (
            <li key={`${row.seq}-${row.callId}`}><span className={`audit-status audit-status--${row.status}`}>{auditLabel(row.status)}</span><code>{row.callId}</code><span>Event #{row.seq}</span><span>epoch {row.policyEpoch ?? "legacy"}</span><span>executor {row.executorGeneration ?? "unknown"}</span></li>
          ))}</ol>
        )}
      </section>
    </section>
  );
}

function Fact({ label, value, code = false }: { label: string; value: string; code?: boolean }) {
  return <div><dt>{label}</dt><dd>{code ? <code>{value}</code> : value}</dd></div>;
}

function readinessLabel(kind: "absent" | "invalid" | "stale" | "ready"): string {
  return { absent: "默认拒绝", invalid: "事实不完整", stale: "已过期", ready: "等待决定" }[kind];
}

function auditLabel(status: "approved" | "rejected" | "invalidated"): string {
  return { approved: "已批准", rejected: "已拒绝", invalidated: "已失效" }[status];
}

function commandFailure(cause: unknown): string {
  if (cause !== null && typeof cause === "object") {
    const code = (cause as Record<string, unknown>).code;
    if (typeof code === "string") return code;
  }
  return "internal";
}
