/** D13/D17/TASK-906 deterministic replay projection with tool-pair integrity. */

import type { ProjectedEvent, ProjectionSnapshot, WireEvent } from "../../lib/projection/index.ts";
import { stableErrorCode } from "../sessions/index.ts";
import type {
  ChatMessage,
  ConversationItem,
  ConversationView,
  ToolCardView,
  ToolErrorView,
} from "./types.ts";

interface AssistantDraft {
  item: Extract<ConversationItem, { kind: "message" }>;
  chunks: string[];
  finalText: string | null;
  closed: boolean;
}

export function projectConversation(snapshot: ProjectionSnapshot): ConversationView {
  const items: ConversationItem[] = [];
  const drafts = new Map<string, AssistantDraft>();
  const tools = new Map<string, Extract<ConversationItem, { kind: "tool" }>>();
  const integrityIssues: string[] = [];
  let currentTurn: number | null = null;
  let activeTurnId: number | null = null;

  for (const projected of snapshot.events) {
    const event = projected.event;
    if (event.type === "turn_started") {
      currentTurn = integerField(event, "turn_id");
      activeTurnId = currentTurn;
    } else if (event.type === "user_message" || event.type === "user_input_queued") {
      items.push(messageItem(projected, currentTurn, "user", textField(event), event.type === "user_input_queued" ? "queued" : "complete"));
    } else if (event.type === "model_chunk_received") {
      appendChunk(projected, currentTurn, items, drafts, integrityIssues);
    } else if (event.type === "assistant_message") {
      finalizeAssistant(projected, currentTurn, items, drafts);
    } else if (event.type === "tool_call_requested") {
      beginTool(projected, currentTurn, items, tools, integrityIssues);
    } else if (event.type === "tool_execution_terminated") {
      terminateTool(projected, tools, integrityIssues);
    } else if (event.type === "tool_result_added") {
      finishTool(projected, tools, integrityIssues);
    } else if (event.type === "turn_aborted") {
      const turnId = integerField(event, "turn_id");
      items.push(messageItem(projected, turnId, "system", "Turn 已中止，详细原因请查看审计时间线。", "complete"));
      if (turnId === activeTurnId) activeTurnId = null;
      markDanglingTools(tools, turnId, integrityIssues);
      interruptDrafts(drafts);
    } else if (event.type === "turn_completed") {
      const turnId = integerField(event, "turn_id");
      if (turnId === activeTurnId) activeTurnId = null;
      markDanglingTools(tools, turnId, integrityIssues);
      interruptDrafts(drafts);
    }
  }

  for (const draft of drafts.values()) {
    draft.item.message.markdown = draft.finalText ?? draft.chunks.join("");
    if (draft.finalText === null && !draft.closed) {
      draft.item.message.state = snapshot.connection === "connected" && activeTurnId !== null ? "streaming" : "interrupted";
    }
  }

  const healthy = integrityIssues.length === 0;
  return {
    sessionId: snapshot.sessionId,
    connection: snapshot.connection,
    activeTurnId,
    items: items.sort((left, right) => left.seq - right.seq),
    integrityIssues,
    canSend: healthy && snapshot.connection === "connected" && activeTurnId === null && snapshot.repair === null,
    canSteer: healthy && snapshot.connection === "connected" && activeTurnId !== null && snapshot.repair === null,
    canCancel: healthy && snapshot.connection === "connected" && activeTurnId !== null,
    canResume: snapshot.connection === "disconnected" || snapshot.repair !== null,
  };
}

export function snapshotConversation(view: ConversationView): string {
  return JSON.stringify({
    session: view.sessionId,
    connection: view.connection,
    activeTurn: view.activeTurnId,
    controls: [view.canSend, view.canSteer, view.canCancel, view.canResume],
    integrity: view.integrityIssues,
    items: view.items.map((item) =>
      item.kind === "message"
        ? ["message", item.message.role, item.message.state, item.message.markdown, item.message.callId]
        : ["tool", item.tool.callId, item.tool.status, item.tool.error?.code ?? null, item.tool.eventSpan],
    ),
  });
}

function appendChunk(
  projected: ProjectedEvent,
  turnId: number | null,
  items: ConversationItem[],
  drafts: Map<string, AssistantDraft>,
  issues: string[],
): void {
  const callId = stringField(projected.event, "call_id");
  if (callId === null) {
    issues.push(`seq ${projected.seq}: model chunk missing call_id`);
    return;
  }
  let draft = drafts.get(callId);
  if (!draft) {
    const item = messageItem(projected, turnId, "assistant", "", "streaming", callId);
    draft = { item, chunks: [], finalText: null, closed: false };
    drafts.set(callId, draft);
    items.push(item);
  }
  if (draft.finalText !== null || draft.closed) {
    issues.push(`seq ${projected.seq}: chunk arrived after finalized call ${callId}`);
    return;
  }
  draft.chunks.push(textField(projected.event, "delta_text"));
}

function finalizeAssistant(
  projected: ProjectedEvent,
  turnId: number | null,
  items: ConversationItem[],
  drafts: Map<string, AssistantDraft>,
): void {
  const open = [...drafts.values()].reverse().find((draft) => draft.finalText === null && !draft.closed);
  if (open) {
    open.finalText = textField(projected.event);
    open.item.message.state = "complete";
    return;
  }
  items.push(messageItem(projected, turnId, "assistant", textField(projected.event), "complete"));
}

function beginTool(
  projected: ProjectedEvent,
  turnId: number | null,
  items: ConversationItem[],
  tools: Map<string, Extract<ConversationItem, { kind: "tool" }>>,
  issues: string[],
): void {
  const callId = stringField(projected.event, "call_id");
  if (callId === null) {
    issues.push(`seq ${projected.seq}: tool call missing call_id`);
    return;
  }
  if (tools.has(callId)) {
    issues.push(`seq ${projected.seq}: duplicate tool call ${callId}`);
    return;
  }
  const item: Extract<ConversationItem, { kind: "tool" }> = {
    kind: "tool",
    seq: projected.seq,
    tool: {
      callId,
      turnId,
      tool: stringField(projected.event, "tool") ?? "unknown_tool",
      args: structuredClone(projected.event.args),
      requestedSeq: projected.seq,
      resultSeq: null,
      eventSpan: null,
      status: "running",
      resultPreview: null,
      error: null,
      audit: ["tool_call_requested"],
    },
  };
  tools.set(callId, item);
  items.push(item);
}

function terminateTool(
  projected: ProjectedEvent,
  tools: Map<string, Extract<ConversationItem, { kind: "tool" }>>,
  issues: string[],
): void {
  const callId = stringField(projected.event, "call_id");
  const item = callId && tools.get(callId);
  if (!item) {
    issues.push(`seq ${projected.seq}: termination without tool call`);
    return;
  }
  const termination = String(projected.event.termination ?? "");
  item.tool.status = termination === "deadline_exceeded" ? "timed_out" : "cancelled";
  item.tool.audit = [...item.tool.audit, `tool_execution_terminated:${termination || "unknown"}`];
}

function finishTool(
  projected: ProjectedEvent,
  tools: Map<string, Extract<ConversationItem, { kind: "tool" }>>,
  issues: string[],
): void {
  const callId = stringField(projected.event, "call_id");
  const item = callId && tools.get(callId);
  if (!item) {
    issues.push(`seq ${projected.seq}: result without tool call`);
    return;
  }
  if (item.tool.resultSeq !== null) {
    issues.push(`seq ${projected.seq}: duplicate tool result ${callId}`);
    return;
  }
  const outcome = objectValue(projected.event.outcome);
  const success = objectField(outcome, "success");
  const failure = objectField(outcome, "failure");
  item.tool.resultSeq = projected.seq;
  item.tool.eventSpan = projected.seq - item.tool.requestedSeq;
  item.tool.audit = [...item.tool.audit, "tool_result_added"];
  if (success) {
    item.tool.status = "success";
    item.tool.resultPreview = previewValue(success.value);
    return;
  }
  const error = objectField(failure, "error");
  item.tool.error = projectToolError(error);
  if (item.tool.status !== "timed_out" && item.tool.status !== "cancelled") {
    item.tool.status = item.tool.error.code === "approval_rejected" || item.tool.error.code === "sandbox_denied" ? "rejected" : "failed";
  }
}

function projectToolError(error: Record<string, unknown> | null): ToolErrorView {
  const code = error ? stringField(error, "code") : null;
  return {
    code: code ? stableErrorCode(code) ?? "unknown" : "unknown",
    message: (error && stringField(error, "message")) ?? "工具返回了未分类错误",
  };
}

function markDanglingTools(
  tools: Map<string, Extract<ConversationItem, { kind: "tool" }>>,
  turnId: number | null,
  issues: string[],
): void {
  for (const item of tools.values()) {
    if (item.tool.turnId === turnId && item.tool.resultSeq === null) {
      issues.push(`turn ${turnId ?? "unknown"}: tool ${item.tool.callId} has no result`);
    }
  }
}

function interruptDrafts(drafts: Map<string, AssistantDraft>): void {
  for (const draft of drafts.values()) {
    if (draft.finalText === null && !draft.closed) {
      draft.closed = true;
      draft.item.message.state = "interrupted";
    }
  }
}

function messageItem(
  projected: ProjectedEvent,
  turnId: number | null,
  role: ChatMessage["role"],
  markdown: string,
  state: ChatMessage["state"],
  callId: string | null = null,
): Extract<ConversationItem, { kind: "message" }> {
  return {
    kind: "message",
    seq: projected.seq,
    message: { id: `${role}-${projected.seq}`, seq: projected.seq, turnId, role, markdown, state, callId },
  };
}

function textField(event: WireEvent, field = "text"): string {
  const value = event[field];
  return typeof value === "string" ? value : "";
}

function stringField(event: Record<string, unknown>, field: string): string | null {
  const value = event[field];
  return typeof value === "string" && value !== "" ? value : null;
}

function integerField(event: WireEvent, field: string): number | null {
  const value = event[field];
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function objectValue(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function objectField(value: Record<string, unknown> | null, field: string): Record<string, unknown> | null {
  return objectValue(value?.[field]);
}

function previewValue(value: unknown): string {
  const encoded = typeof value === "string" ? value : JSON.stringify(value);
  if (!encoded) return "完成，无可显示结果";
  return encoded.length > 240 ? `${encoded.slice(0, 237)}…` : encoded;
}
