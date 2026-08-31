/** D13/D25/TASK-905 deterministic UI state without a client-side truth store. */

import type {
  CommandErrorDto,
  ErrorPresentation,
  OperationAction,
  OperationState,
  SessionCollectionState,
  SessionOperation,
  StableErrorCode,
} from "./types.ts";

const STABLE_CODES = new Set<StableErrorCode>([
  "tool_args_invalid",
  "sandbox_denied",
  "approval_rejected",
  "context_window_exceeded",
  "model_stream_broken",
  "subagent_cancelled",
  "session_not_found",
  "cursor_invalid",
  "team_revision_conflict",
  "team_dependency_cycle",
  "tool_timeout",
  "tool_loop_detected",
  "file_revision_conflict",
  "internal",
]);

export function operationReducer(state: OperationState, action: OperationAction): OperationState {
  if (action.type === "dismiss") return { kind: "idle" };
  if (action.type === "request") {
    return isDestructive(action.operation)
      ? { kind: "confirming", operation: action.operation }
      : { kind: "submitting", operation: action.operation };
  }
  if (action.type === "confirm") {
    return state.kind === "confirming"
      ? { kind: "submitting", operation: state.operation }
      : state;
  }
  if (action.type === "receipt") {
    return state.kind === "submitting"
      ? { kind: "awaiting_projection", operation: state.operation, receipt: action.receipt }
      : state;
  }
  if (action.type === "failed") {
    return state.kind === "submitting"
      ? { kind: "failed", operation: state.operation, error: action.error }
      : state;
  }
  if (state.kind !== "awaiting_projection") return state;
  const receipt = state.receipt;
  return action.sessionId === receipt.sessionId &&
    action.generation === receipt.generation &&
    action.eventCount >= receipt.eventCount
    ? { kind: "idle" }
    : state;
}

export function classifyError(error: CommandErrorDto): ErrorPresentation {
  const code = stableErrorCode(error.code);
  switch (code) {
    case "cursor_invalid":
      return { title: "客户端代际已变化", action: "refresh", tone: "warning" };
    case "session_not_found":
      return { title: "会话不存在或已移动", action: "refresh", tone: "warning" };
    case "model_stream_broken":
    case "tool_timeout":
    case "internal":
      return { title: "服务暂时不可用", action: "retry", tone: "warning" };
    case "sandbox_denied":
    case "approval_rejected":
      return { title: "操作被安全策略拒绝", action: "none", tone: "danger" };
    case "context_window_exceeded":
      return { title: "上下文额度不足", action: "open_settings", tone: "warning" };
    case null:
      return { title: "发生未知协议错误", action: "none", tone: "danger" };
    default:
      return { title: "操作未完成", action: "none", tone: "danger" };
  }
}

export function stableErrorCode(code: string): StableErrorCode | null {
  return STABLE_CODES.has(code as StableErrorCode) ? (code as StableErrorCode) : null;
}

export function operationLabel(operation: SessionOperation): string {
  switch (operation.kind) {
    case "create":
      return `创建 ${operation.sessionId}`;
    case "resume":
      return `恢复 ${operation.sessionId}`;
    case "fork":
      return `从 ${operation.sourceId} 派生 ${operation.targetId}`;
    case "revert":
      return `将 ${operation.sourceId} 回退为 ${operation.targetId}`;
  }
}

export function snapshotSessionState(state: SessionCollectionState): string {
  switch (state.kind) {
    case "loading":
      return "loading";
    case "forbidden":
      return "forbidden";
    case "error":
      return `error:${stableErrorCode(state.error.code) ?? "unknown"}`;
    case "ready":
    case "disconnected": {
      const sessions = state.sessions
        .map((session) => `${session.sessionId}:${session.health}:${session.latestTurnStatus ?? "empty"}`)
        .join("|");
      return `${state.kind}:${state.selectedId ?? "none"}:${sessions || "empty"}`;
    }
  }
}

function isDestructive(
  operation: SessionOperation,
): operation is Extract<SessionOperation, { kind: "fork" | "revert" }> {
  return operation.kind === "fork" || operation.kind === "revert";
}
