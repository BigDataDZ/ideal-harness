/** D5/D13/D23/TASK-907 read-only workspace view projected from paired tool events. */

import type { ProjectionSnapshot, WireEvent } from "../../lib/projection/index.ts";
import { stableErrorCode } from "../sessions/index.ts";
import type { FileChange, ObservedFile, WorkspaceView } from "./types.ts";

interface ToolCall {
  callId: string;
  tool: string;
  seq: number;
  args: Record<string, unknown>;
}

export function projectWorkspace(snapshot: ProjectionSnapshot | null): WorkspaceView {
  if (!snapshot) return { files: [], changes: [], integrityIssues: [] };
  const calls = new Map<string, ToolCall>();
  const reads = new Map<string, ObservedFile>();
  const changes = new Map<string, FileChange>();
  const issues: string[] = [];

  for (const projected of snapshot.events) {
    const event = projected.event;
    if (event.type === "tool_call_requested") {
      const callId = stringField(event, "call_id");
      const tool = stringField(event, "tool");
      const args = objectValue(event.args);
      if (!callId || !tool || !args) continue;
      if (calls.has(callId)) {
        issues.push(`Event #${projected.seq}: duplicate tool call ${callId}`);
        continue;
      }
      const call = { callId, tool, seq: projected.seq, args };
      calls.set(callId, call);
      if (tool === "fs_write" || tool === "fs_edit") {
        const change = beginChange(call, reads, issues);
        if (change) changes.set(callId, change);
      }
    } else if (event.type === "tool_result_added") {
      applyResult(projected.seq, event, calls, reads, changes, issues);
    }
  }
  return {
    files: [...reads.values()].sort((left, right) => left.path.localeCompare(right.path)),
    changes: [...changes.values()].sort((left, right) => right.requestSeq - left.requestSeq),
    integrityIssues: issues,
  };
}

export function normalizeWorkspacePath(input: string): string | null {
  if (input === "" || input.length > 1024 || /[\u0000-\u001f\u007f]/.test(input)) return null;
  const normalized = input.replaceAll("\\", "/");
  if (normalized.startsWith("/") || normalized.startsWith("//") || /^[A-Za-z]:\//.test(normalized)) return null;
  const segments = normalized.split("/");
  if (segments.some((segment) => segment === "" || segment === "." || segment === "..")) return null;
  return segments.join("/");
}

function beginChange(
  call: ToolCall,
  reads: Map<string, ObservedFile>,
  issues: string[],
): FileChange | null {
  const rawPath = stringField(call.args, "path");
  const path = rawPath && normalizeWorkspacePath(rawPath);
  if (!path) {
    issues.push(`Event #${call.seq}: unsafe workspace path rejected`);
    return null;
  }
  const observed = reads.get(path);
  const expectedHash = stringField(call.args, "expected_hash");
  const before = observed?.content ?? null;
  const after = call.tool === "fs_write"
    ? stringField(call.args, "content")
    : applyEdit(before, call.args);
  return {
    callId: call.callId,
    tool: call.tool as FileChange["tool"],
    path,
    requestSeq: call.seq,
    resultSeq: null,
    status: "pending",
    expectedHash,
    observedHash: observed?.hash ?? null,
    casVerified: expectedHash !== null && observed?.hash === expectedHash,
    before,
    after,
    errorCode: null,
  };
}

function applyResult(
  seq: number,
  event: WireEvent,
  calls: Map<string, ToolCall>,
  reads: Map<string, ObservedFile>,
  changes: Map<string, FileChange>,
  issues: string[],
): void {
  const callId = stringField(event, "call_id");
  const call = callId && calls.get(callId);
  if (!call) return;
  const outcome = objectValue(event.outcome);
  const success = objectField(outcome, "success");
  const failure = objectField(outcome, "failure");
  if (call.tool === "fs_read" && success) projectRead(seq, call, success, reads, issues);
  if (call.tool === "fs_glob" && success) projectGlob(seq, success, reads, issues);
  const change = changes.get(call.callId);
  if (!change) return;
  if (change.resultSeq !== null) {
    issues.push(`Event #${seq}: duplicate tool result ${call.callId}`);
    return;
  }
  change.resultSeq = seq;
  change.status = success ? "applied" : "failed";
  const error = objectField(failure, "error");
  const code = stringField(error, "code");
  change.errorCode = code ? stableErrorCode(code) ?? "unknown" : null;
  if (success && change.after !== null) {
    reads.set(change.path, {
      path: change.path,
      content: change.after,
      hash: null,
      sourceSeq: seq,
      truncated: false,
    });
  }
}

function projectRead(
  seq: number,
  call: ToolCall,
  success: Record<string, unknown>,
  reads: Map<string, ObservedFile>,
  issues: string[],
): void {
  const pathValue = stringField(call.args, "path");
  const path = pathValue && normalizeWorkspacePath(pathValue);
  if (!path) {
    issues.push(`Event #${call.seq}: unsafe read path rejected`);
    return;
  }
  const value = objectValue(success.value);
  if (!value) return;
  const content = stringField(value, "content");
  reads.set(path, {
    path,
    content,
    hash: stringField(value, "hash"),
    sourceSeq: seq,
    truncated: value.truncated === true,
  });
}

function projectGlob(
  seq: number,
  success: Record<string, unknown>,
  reads: Map<string, ObservedFile>,
  issues: string[],
): void {
  const value = objectValue(success.value);
  const matches = value?.matches;
  if (!Array.isArray(matches)) return;
  for (const raw of matches) {
    if (typeof raw !== "string") continue;
    const path = normalizeWorkspacePath(raw);
    if (!path) {
      issues.push(`Event #${seq}: unsafe glob result path rejected`);
      continue;
    }
    if (!reads.has(path)) reads.set(path, { path, content: null, hash: null, sourceSeq: seq, truncated: false });
  }
}

function applyEdit(before: string | null, args: Record<string, unknown>): string | null {
  if (before === null) return null;
  const oldText = stringField(args, "old_string");
  const newText = stringField(args, "new_string");
  if (oldText === null || newText === null || oldText === "") return null;
  if (args.replace_all === true) return before.split(oldText).join(newText);
  const first = before.indexOf(oldText);
  if (first < 0 || before.indexOf(oldText, first + oldText.length) >= 0) return null;
  return `${before.slice(0, first)}${newText}${before.slice(first + oldText.length)}`;
}

function objectValue(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function objectField(value: Record<string, unknown> | null, field: string): Record<string, unknown> | null {
  return objectValue(value?.[field]);
}

function stringField(value: Record<string, unknown> | null, field: string): string | null {
  const candidate = value?.[field];
  return typeof candidate === "string" ? candidate : null;
}
