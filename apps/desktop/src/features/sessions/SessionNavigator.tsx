import { useId, useState, type FormEvent, type KeyboardEvent } from "react";

import { classifyError, operationLabel } from "./session-state.ts";
import type {
  OperationState,
  SessionCollectionState,
  SessionOperation,
  SessionSummary,
} from "./types.ts";

interface SessionNavigatorProps {
  state: SessionCollectionState;
  operation: OperationState;
  onSelect: (sessionId: string) => void;
  onRequestOperation: (operation: SessionOperation) => void;
  onConfirmOperation: () => void;
  onDismissOperation: () => void;
  onRetry: () => void;
}

export function SessionNavigator({
  state,
  operation,
  onSelect,
  onRequestOperation,
  onConfirmOperation,
  onDismissOperation,
  onRetry,
}: SessionNavigatorProps) {
  const [sessionId, setSessionId] = useState("");
  const createId = useId();
  const canWrite = state.kind === "ready" && operation.kind === "idle";

  const submitCreate = (event: FormEvent) => {
    event.preventDefault();
    const normalized = sessionId.trim();
    if (/^[A-Za-z0-9_-]+$/.test(normalized)) {
      onRequestOperation({ kind: "create", sessionId: normalized });
      setSessionId("");
    }
  };

  return (
    <aside className="session-nav" aria-labelledby="sessions-title">
      <header className="panel-heading">
        <div>
          <p className="panel-kicker">SESSIONS</p>
          <h1 id="sessions-title">会话</h1>
        </div>
        <span className="count-badge">{sessionCount(state)}</span>
      </header>

      <form className="new-session" onSubmit={submitCreate}>
        <label className="sr-only" htmlFor={createId}>
          新会话标识
        </label>
        <input
          id={createId}
          value={sessionId}
          onChange={(event) => setSessionId(event.currentTarget.value)}
          placeholder="新会话 ID"
          pattern="[A-Za-z0-9_\-]+"
          autoComplete="off"
          disabled={!canWrite}
        />
        <button type="submit" disabled={!canWrite || sessionId.trim() === ""}>
          新建
        </button>
      </form>

      <SessionStateBody
        state={state}
        onSelect={onSelect}
        onRequestOperation={onRequestOperation}
        onRetry={onRetry}
      />
      <OperationNotice
        operation={operation}
        onConfirm={onConfirmOperation}
        onDismiss={onDismissOperation}
      />
    </aside>
  );
}

function SessionStateBody({
  state,
  onSelect,
  onRequestOperation,
  onRetry,
}: Pick<SessionNavigatorProps, "state" | "onSelect" | "onRequestOperation" | "onRetry">) {
  if (state.kind === "loading") {
    return <EmptyNotice title="正在读取会话" detail="等待 Rust 宿主返回权威状态…" busy />;
  }
  if (state.kind === "forbidden") {
    return <EmptyNotice title="没有访问权限" detail={state.message} tone="danger" />;
  }
  if (state.kind === "error") {
    const presentation = classifyError(state.error);
    return (
      <EmptyNotice title={presentation.title} detail={state.error.message} tone={presentation.tone}>
        {presentation.action === "retry" || presentation.action === "refresh" ? (
          <button type="button" className="text-button" onClick={onRetry}>
            {presentation.action === "refresh" ? "刷新" : "重试"}
          </button>
        ) : null}
      </EmptyNotice>
    );
  }

  const disconnected = state.kind === "disconnected";
  return (
    <>
      {disconnected ? (
        <div className="inline-alert" role="alert">
          连接已中断，当前内容仅为最后一次事件投影，恢复前禁止写操作。
        </div>
      ) : null}
      {state.sessions.length === 0 ? (
        <EmptyNotice title="还没有会话" detail="创建会话后，状态将由事件流持续投影。" />
      ) : (
        <div
          className="session-list"
          role="listbox"
          aria-label="会话列表"
          onKeyDown={moveListFocus}
        >
          {state.sessions.map((session) => (
            <SessionRow
              key={session.sessionId}
              session={session}
              selected={session.sessionId === state.selectedId}
              disconnected={disconnected}
              onSelect={onSelect}
              onRequestOperation={onRequestOperation}
            />
          ))}
        </div>
      )}
    </>
  );
}

function SessionRow({
  session,
  selected,
  disconnected,
  onSelect,
  onRequestOperation,
}: {
  session: SessionSummary;
  selected: boolean;
  disconnected: boolean;
  onSelect: (sessionId: string) => void;
  onRequestOperation: (operation: SessionOperation) => void;
}) {
  const unavailable = disconnected || session.health === "corrupt";
  return (
    <article className={`session-row${selected ? " session-row--selected" : ""}`}>
      <button
        type="button"
        role="option"
        aria-selected={selected}
        className="session-select"
        onClick={() => onSelect(session.sessionId)}
      >
        <span className="session-name">{session.sessionId}</span>
        <span className={`turn-state turn-state--${session.latestTurnStatus ?? "empty"}`}>
          {session.health === "corrupt"
            ? "会话损坏"
            : turnStatusLabel(session.latestTurnStatus)}
        </span>
        <span className="session-meta">
          {session.eventCount} events · gen {session.generation}
        </span>
      </button>
      <div className="session-actions" aria-label={`${session.sessionId} 操作`}>
        <button
          type="button"
          disabled={unavailable}
          onClick={() => onRequestOperation({ kind: "resume", sessionId: session.sessionId })}
        >
          Resume
        </button>
        <button
          type="button"
          disabled={unavailable}
          onClick={() =>
            onRequestOperation({
              kind: "fork",
              sourceId: session.sessionId,
              targetId: `${session.sessionId}-fork`,
              boundary: session.eventCount,
            })
          }
        >
          Fork
        </button>
        <button
          type="button"
          disabled={unavailable || session.latestTurnId === null}
          onClick={() => {
            if (session.latestTurnId !== null) {
              onRequestOperation({
                kind: "revert",
                sourceId: session.sessionId,
                targetId: `${session.sessionId}-revert`,
                turnId: session.latestTurnId,
              });
            }
          }}
        >
          Revert
        </button>
      </div>
    </article>
  );
}

function OperationNotice({
  operation,
  onConfirm,
  onDismiss,
}: {
  operation: OperationState;
  onConfirm: () => void;
  onDismiss: () => void;
}) {
  if (operation.kind === "idle") return null;
  if (operation.kind === "confirming") {
    return (
      <div className="confirm-card" role="dialog" aria-modal="true" aria-labelledby="confirm-title">
        <h2 id="confirm-title">确认派生会话？</h2>
        <p>{operationLabel(operation.operation)}</p>
        <p className="muted">原事件不会被删除；结果必须等待宿主回执和事件投影确认。</p>
        <div className="confirm-actions">
          <button type="button" onClick={onDismiss}>
            取消
          </button>
          <button type="button" className="danger-button" onClick={onConfirm} autoFocus>
            二次确认
          </button>
        </div>
      </div>
    );
  }
  if (operation.kind === "failed") {
    const presentation = classifyError(operation.error);
    return (
      <div className={`operation-banner operation-banner--${presentation.tone}`} role="alert">
        <strong>{presentation.title}</strong>
        <span>{operation.error.message}</span>
        <button type="button" onClick={onDismiss}>
          关闭
        </button>
      </div>
    );
  }
  return (
    <div className="operation-banner" role="status" aria-live="polite">
      <strong>{operation.kind === "submitting" ? "正在提交" : "等待事件确认"}</strong>
      <span>{operationLabel(operation.operation)}</span>
      {operation.kind === "awaiting_projection" ? (
        <span>宿主已记录 {operation.receipt.eventCount} 个事件，尚未乐观更新列表。</span>
      ) : null}
    </div>
  );
}

function EmptyNotice({
  title,
  detail,
  tone = "neutral",
  busy = false,
  children,
}: {
  title: string;
  detail: string;
  tone?: "neutral" | "warning" | "danger";
  busy?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className={`empty-notice empty-notice--${tone}`} role="status" aria-busy={busy}>
      <span className="empty-glyph" aria-hidden="true" />
      <strong>{title}</strong>
      <p>{detail}</p>
      {children}
    </div>
  );
}

function sessionCount(state: SessionCollectionState): number {
  return state.kind === "ready" || state.kind === "disconnected" ? state.sessions.length : 0;
}

function turnStatusLabel(status: SessionSummary["latestTurnStatus"]): string {
  if (status === "active") return "运行中";
  if (status === "completed") return "已完成";
  if (status === "aborted") return "已中止";
  return "空会话";
}

function moveListFocus(event: KeyboardEvent<HTMLDivElement>): void {
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
  const items = [...event.currentTarget.querySelectorAll<HTMLButtonElement>(".session-select")];
  if (items.length === 0) return;
  const current = items.indexOf(document.activeElement as HTMLButtonElement);
  let next = current;
  if (event.key === "Home") next = 0;
  if (event.key === "End") next = items.length - 1;
  if (event.key === "ArrowDown") next = current < 0 ? 0 : (current + 1) % items.length;
  if (event.key === "ArrowUp") next = current <= 0 ? items.length - 1 : current - 1;
  event.preventDefault();
  items[next]?.focus();
}
