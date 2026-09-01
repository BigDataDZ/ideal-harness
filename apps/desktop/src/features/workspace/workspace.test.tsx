import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { ProjectedEvent, ProjectionSnapshot } from "../../lib/projection/index.ts";
import { buildLineDiff } from "./diff.ts";
import { normalizeWorkspacePath, projectWorkspace } from "./projection.ts";
import { WorkspacePanel } from "./WorkspacePanel.tsx";

test("workspace paths reject traversal, absolute paths and control characters", () => {
  for (const unsafe of ["../secret", "a/../../secret", "/etc/passwd", "C:\\secret", "//server/share", "a\0b", "a//b", "./a"]) {
    assert.ok(normalizeWorkspacePath(unsafe) === null);
  }
  assert.equal(normalizeWorkspacePath("src\\main.rs"), "src/main.rs");
});

test("paired fs events produce read-only preview and an event-addressable CAS diff", () => {
  const events = [
    call(1, "read-1", "fs_read", { path: "src/main.rs" }),
    result(2, "read-1", { success: { value: { path: "D:/work/src/main.rs", content: "fn main() {\n  old();\n}", hash: "hash-a" } } }),
    call(3, "edit-1", "fs_edit", { path: "src/main.rs", old_string: "old();", new_string: "new();", expected_hash: "hash-a" }),
    result(4, "edit-1", { success: { value: { path: "D:/work/src/main.rs", replacements: 1 } } }),
  ];
  const view = projectWorkspace(snapshot(events));
  assert.equal(view.integrityIssues.length, 0);
  assert.equal(view.files[0]?.content, "fn main() {\n  new();\n}");
  assert.deepEqual(view.changes.map((change) => [change.path, change.status, change.casVerified, change.requestSeq, change.resultSeq]), [
    ["src/main.rs", "applied", true, 3, 4],
  ]);
  const diff = buildLineDiff(view.changes[0]?.before ?? null, view.changes[0]?.after ?? null);
  assert.ok(diff.some((line) => line.kind === "removed" && line.text.includes("old")));
  assert.ok(diff.some((line) => line.kind === "added" && line.text.includes("new")));

  const markup = renderToStaticMarkup(<WorkspacePanel snapshot={snapshot(events)} />);
  for (const text of ["src/main.rs", "Event", "#3", "#4", "expected_hash", "hash-a", "与最近 fs_read hash 一致"]) {
    assert.ok(markup.includes(text));
  }
  assert.ok(!/<button[^>]*>[^<]*保存/.test(markup));
  assert.ok(!markup.includes("textarea"));
});

test("unsafe requests and failed symlink escape attempts never become file previews", () => {
  const events = [
    call(1, "escape", "fs_read", { path: "../outside.txt" }),
    result(2, "escape", { success: { value: { content: "secret", hash: "bad" } } }),
    call(3, "symlink", "fs_read", { path: "linked/outside.txt" }),
    result(4, "symlink", { failure: { error: { code: "sandbox_denied", message: "symlink escaped root" } } }),
    call(5, "glob", "fs_glob", { pattern: "**/*" }),
    result(6, "glob", { success: { value: { matches: ["safe.txt", "../leak.txt", "C:/leak.txt"] } } }),
  ];
  const view = projectWorkspace(snapshot(events));
  assert.deepEqual(view.files.map((file) => file.path), ["safe.txt"]);
  assert.ok(view.integrityIssues.length >= 2);
  assert.ok(!JSON.stringify(view).includes("secret"));
});

test("missing or mismatched expected_hash remains visible but is never claimed as verified", () => {
  const events = [
    call(1, "read", "fs_read", { path: "a.txt" }),
    result(2, "read", { success: { value: { content: "old", hash: "current" } } }),
    call(3, "write", "fs_write", { path: "a.txt", content: "new", expected_hash: "stale" }),
    result(4, "write", { failure: { error: { code: "file_revision_conflict", message: "changed" } } }),
  ];
  const change = projectWorkspace(snapshot(events)).changes[0];
  assert.equal(change?.casVerified, false);
  assert.equal(change?.status, "failed");
  assert.equal(change?.errorCode, "file_revision_conflict");
});

function call(seq: number, callId: string, tool: string, args: Record<string, unknown>): ProjectedEvent {
  return projected(seq, { type: "tool_call_requested", call_id: callId, tool, args });
}

function result(seq: number, callId: string, outcome: Record<string, unknown>): ProjectedEvent {
  return projected(seq, { type: "tool_result_added", call_id: callId, outcome });
}

function projected(seq: number, event: Record<string, unknown>): ProjectedEvent {
  return { seq, known: true, event: { type: String(event.type), ...event } };
}

function snapshot(events: ProjectedEvent[]): ProjectionSnapshot {
  return {
    sessionId: "demo",
    connectionGeneration: 4,
    events,
    turns: [],
    lastEventId: String(events.at(-1)?.seq ?? 0),
    timelineCursor: 0,
    timelineComplete: true,
    connection: "connected",
    repair: null,
  };
}
