/** D25/TASK-905 session navigation types. Server projections remain authoritative. */

export type StableErrorCode =
  | "tool_args_invalid"
  | "sandbox_denied"
  | "approval_rejected"
  | "context_window_exceeded"
  | "model_stream_broken"
  | "subagent_cancelled"
  | "session_not_found"
  | "cursor_invalid"
  | "team_revision_conflict"
  | "team_dependency_cycle"
  | "tool_timeout"
  | "tool_loop_detected"
  | "file_revision_conflict"
  | "internal";

export interface CommandErrorDto {
  code: string;
  message: string;
}

export interface SessionSummary {
  sessionId: string;
  eventCount: number;
  generation: number;
  latestTurnId: number | null;
  latestTurnStatus: "active" | "completed" | "aborted" | null;
  health: "healthy" | "corrupt";
}

export type SessionCollectionState =
  | { kind: "loading" }
  | { kind: "ready"; sessions: readonly SessionSummary[]; selectedId: string | null }
  | { kind: "disconnected"; sessions: readonly SessionSummary[]; selectedId: string | null }
  | { kind: "forbidden"; message: string }
  | { kind: "error"; error: CommandErrorDto };

export type SessionOperation =
  | { kind: "create"; sessionId: string }
  | { kind: "resume"; sessionId: string }
  | { kind: "fork"; sourceId: string; targetId: string; boundary: number | null }
  | { kind: "revert"; sourceId: string; targetId: string; turnId: number };

export interface SessionReceiptDto {
  sessionId: string;
  eventCount: number;
  generation: number;
}

export type OperationState =
  | { kind: "idle" }
  | { kind: "confirming"; operation: Extract<SessionOperation, { kind: "fork" | "revert" }> }
  | { kind: "submitting"; operation: SessionOperation }
  | { kind: "awaiting_projection"; operation: SessionOperation; receipt: SessionReceiptDto }
  | { kind: "failed"; operation: SessionOperation; error: CommandErrorDto };

export type OperationAction =
  | { type: "request"; operation: SessionOperation }
  | { type: "confirm" }
  | { type: "receipt"; receipt: SessionReceiptDto }
  | { type: "projection_observed"; sessionId: string; eventCount: number; generation: number }
  | { type: "failed"; error: CommandErrorDto }
  | { type: "dismiss" };

export interface ErrorPresentation {
  title: string;
  action: "retry" | "refresh" | "open_settings" | "none";
  tone: "warning" | "danger";
}
