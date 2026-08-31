/** D13/D19/TASK-905 event-derived timeline and diagnostics projection. */

import type { ProjectedEvent, ProjectionSnapshot, WireEvent } from "../../lib/projection/index.ts";
import { classifyError, stableErrorCode } from "../sessions/session-state.ts";
import type { CommandErrorDto } from "../sessions/types.ts";

export interface TimelineRow {
  seq: number;
  type: string;
  label: string;
  summary: string;
  known: boolean;
  tone: "neutral" | "positive" | "warning" | "danger";
}

export interface DiagnosticError extends CommandErrorDto {
  seq: number;
  title: string;
  action: ReturnType<typeof classifyError>["action"];
}

export interface TeamTaskView {
  taskId: string;
  owner: string;
  revision: number;
  status: string;
  blockedBy: readonly string[];
  writeScopes: readonly string[];
}

export interface TimelineView {
  sessionId: string;
  connection: ProjectionSnapshot["connection"];
  generation: number | null;
  lastEventId: string | null;
  repairReason: string | null;
  activeTurns: number;
  completedTurns: number;
  abortedTurns: number;
  tokenUsage: number;
  tokenBudget: number | null;
  tokenRemaining: number | null;
  tokenSources: { provider: number; heuristic: number };
  teamMembers: readonly string[];
  teamTasks: readonly TeamTaskView[];
  teamMessagesPending: number;
  teamConflicts: number;
  errors: readonly DiagnosticError[];
  rows: readonly TimelineRow[];
}

export function buildTimelineView(snapshot: ProjectionSnapshot): TimelineView {
  let tokenBudget: number | null = null;
  const usage = new Map<string, { total: number; source: string }>();
  const members = new Map<string, string>();
  const tasks = new Map<string, TeamTaskView>();
  const pendingMessages = new Set<string>();
  const errors: DiagnosticError[] = [];
  let teamConflicts = 0;

  for (const projected of snapshot.events) {
    const event = projected.event;
    if (event.type === "token_budget_configured") {
      tokenBudget = nonNegativeInteger(event.token_budget) ?? tokenBudget;
    } else if (event.type === "token_usage_recorded") {
      const id = stringField(event, "usage_id");
      const total = nonNegativeInteger(event.total_tokens);
      if (id !== null && total !== null) usage.set(id, { total, source: String(event.source) });
    } else if (event.type === "team_member_registered") {
      const member = objectField(event, "member");
      const id = member && stringField(member, "member_id");
      if (id) members.set(id, stringField(member, "parent_id") ?? "root");
    } else if (event.type === "team_task_created" || event.type === "team_task_updated") {
      const task = projectTeamTask(objectField(event, "task"));
      const previous = task ? tasks.get(task.taskId) : undefined;
      if (task && (!previous || task.revision >= previous.revision)) tasks.set(task.taskId, task);
    } else if (event.type === "team_message_enqueued") {
      const message = objectField(event, "message");
      const id = message && stringField(message, "message_id");
      if (id) pendingMessages.add(id);
    } else if (event.type === "team_message_delivered") {
      const id = stringField(event, "message_id");
      if (id) pendingMessages.delete(id);
    } else if (event.type === "team_write_scope_conflict_detected") {
      teamConflicts += 1;
    }
    const error = projectError(projected.seq, event);
    if (error) errors.push(error);
  }

  const tokenUsage = [...usage.values()].reduce((sum, item) => sum + item.total, 0);
  const provider = [...usage.values()]
    .filter((item) => item.source === "provider")
    .reduce((sum, item) => sum + item.total, 0);
  const heuristic = tokenUsage - provider;
  return {
    sessionId: snapshot.sessionId,
    connection: snapshot.connection,
    generation: snapshot.connectionGeneration,
    lastEventId: snapshot.lastEventId,
    repairReason: snapshot.repair?.reason ?? null,
    activeTurns: snapshot.turns.filter((turn) => turn.status === "active").length,
    completedTurns: snapshot.turns.filter((turn) => turn.status === "completed").length,
    abortedTurns: snapshot.turns.filter((turn) => turn.status === "aborted").length,
    tokenUsage,
    tokenBudget,
    tokenRemaining: tokenBudget === null ? null : Math.max(0, tokenBudget - tokenUsage),
    tokenSources: { provider, heuristic },
    teamMembers: [...members.keys()].sort(),
    teamTasks: [...tasks.values()].sort((left, right) => left.taskId.localeCompare(right.taskId)),
    teamMessagesPending: pendingMessages.size,
    teamConflicts,
    errors,
    rows: snapshot.events.map(projectTimelineRow).reverse(),
  };
}

export function snapshotTimelineView(view: TimelineView): string {
  return JSON.stringify({
    session: view.sessionId,
    connection: view.connection,
    generation: view.generation,
    turns: [view.activeTurns, view.completedTurns, view.abortedTurns],
    tokens: [view.tokenUsage, view.tokenBudget, view.tokenRemaining],
    team: [view.teamMembers.length, view.teamTasks.length, view.teamMessagesPending, view.teamConflicts],
    errors: view.errors.map((error) => [error.seq, error.code, error.action]),
    rows: view.rows.map((row) => [row.seq, row.type, row.known]),
  });
}

function projectTimelineRow(projected: ProjectedEvent): TimelineRow {
  const event = projected.event;
  const [label, summary, tone] = eventPresentation(event);
  return {
    seq: projected.seq,
    type: event.type,
    label: projected.known ? label : "未知事件",
    summary: projected.known ? summary : `保留 ${event.type}，未参与状态归约`,
    known: projected.known,
    tone: projected.known ? tone : "warning",
  };
}

function eventPresentation(event: WireEvent): [string, string, TimelineRow["tone"]] {
  switch (event.type) {
    case "turn_started":
      return ["Turn 开始", `#${nonNegativeInteger(event.turn_id) ?? "?"}`, "neutral"];
    case "turn_completed":
      return ["Turn 完成", `#${nonNegativeInteger(event.turn_id) ?? "?"}`, "positive"];
    case "turn_aborted":
      return ["Turn 中止", `#${nonNegativeInteger(event.turn_id) ?? "?"}`, "danger"];
    case "user_message":
      return ["用户消息", clipped(stringField(event, "text")), "neutral"];
    case "assistant_message":
      return ["助手消息", clipped(stringField(event, "text")), "positive"];
    case "model_chunk_received":
      return ["流式增量", stringField(event, "call_id") ?? "无 call id", "neutral"];
    case "tool_call_requested":
      return ["工具调用", stringField(event, "tool") ?? "未知工具", "warning"];
    case "tool_result_added": {
      const failure = objectField(objectField(event, "outcome"), "failure");
      const error = objectField(failure, "error");
      return error
        ? ["工具失败", stringField(error, "code") ?? "unknown", "danger"]
        : ["工具结果", stringField(event, "call_id") ?? "已返回", "positive"];
    }
    case "token_usage_recorded":
      return ["Token 用量", `${nonNegativeInteger(event.total_tokens) ?? 0} tokens`, "neutral"];
    case "team_task_created":
    case "team_task_updated": {
      const task = objectField(event, "task");
      return ["Team 任务", (task && stringField(task, "task_id")) ?? "未知任务", "neutral"];
    }
    case "team_write_scope_conflict_detected":
      return ["写范围冲突", "需要协调 Agent Team", "danger"];
    case "approval_decided":
      return ["审批决定", event.approved === true ? "已批准" : "已拒绝", event.approved === true ? "positive" : "danger"];
    default:
      return [humanize(event.type), "已记录到事件流", "neutral"];
  }
}

function projectError(seq: number, event: WireEvent): DiagnosticError | null {
  if (event.type !== "tool_result_added") return null;
  const outcome = objectField(event, "outcome");
  const failure = objectField(outcome, "failure");
  const error = objectField(failure, "error");
  if (!error) return null;
  const code = stringField(error, "code") ?? "unknown";
  const message = stringField(error, "message") ?? "未提供错误说明";
  const presentation = classifyError({ code, message });
  return {
    seq,
    code: stableErrorCode(code) ?? "unknown",
    message,
    title: presentation.title,
    action: presentation.action,
  };
}

function projectTeamTask(value: Record<string, unknown> | null): TeamTaskView | null {
  if (!value) return null;
  const taskId = stringField(value, "task_id");
  const owner = stringField(value, "owner_member_id");
  const revision = nonNegativeInteger(value.revision);
  const status = stringField(value, "status");
  if (taskId === null || owner === null || revision === null || status === null) return null;
  return {
    taskId,
    owner,
    revision,
    status,
    blockedBy: stringArray(value.blocked_by),
    writeScopes: stringArray(value.write_scopes),
  };
}

function objectField(
  value: Record<string, unknown> | null,
  field: string,
): Record<string, unknown> | null {
  const candidate = value?.[field];
  return candidate !== null && typeof candidate === "object" && !Array.isArray(candidate)
    ? (candidate as Record<string, unknown>)
    : null;
}

function stringField(value: Record<string, unknown>, field: string): string | null {
  const candidate = value[field];
  return typeof candidate === "string" && candidate !== "" ? candidate : null;
}

function nonNegativeInteger(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function clipped(value: string | null): string {
  if (!value) return "无文本";
  return value.length > 64 ? `${value.slice(0, 61)}…` : value;
}

function humanize(type: string): string {
  return type
    .split("_")
    .filter(Boolean)
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(" ");
}
