import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { ProjectedEvent, ProjectionSnapshot } from "../../lib/projection/index.ts";
import { ChatPanel } from "./ChatPanel.tsx";
import { projectConversation, snapshotConversation } from "./conversation.ts";
import { prepareMarkdown, SafeMarkdown, safeUrlTransform } from "./SafeMarkdown.tsx";

const projected = (seq: number, event: Record<string, unknown>): ProjectedEvent => ({ seq, event: { type: String(event.type), ...event }, known: true });

const snapshot = (
  events: ProjectedEvent[],
  connection: ProjectionSnapshot["connection"] = "connected",
): ProjectionSnapshot => ({
  sessionId: "demo",
  connectionGeneration: 4,
  connection,
  events,
  turns: [],
  lastEventId: events.length === 0 ? null : String(events.at(-1)?.seq),
  timelineCursor: 0,
  timelineComplete: true,
  repair: null,
});

test("stream chunks collapse into one final assistant message without replay duplication", () => {
  const events = [
    projected(0, { type: "turn_started", turn_id: 1 }),
    projected(1, { type: "user_message", text: "run" }),
    projected(2, { type: "model_chunk_received", call_id: "m1", delta_text: "hel" }),
    projected(3, { type: "model_chunk_received", call_id: "m1", delta_text: "lo" }),
    projected(4, { type: "assistant_message", text: "hello" }),
    projected(5, { type: "tool_call_requested", call_id: "c1", tool: "fs_read", args: { path: "a" } }),
    projected(6, { type: "tool_result_added", call_id: "c1", outcome: { success: { value: "ok" } } }),
    projected(7, { type: "turn_completed", turn_id: 1 }),
  ];
  const view = projectConversation(snapshot(events));
  assert.equal(view.items.filter((item) => item.kind === "message" && item.message.role === "assistant").length, 1);
  const assistant = view.items.find((item) => item.kind === "message" && item.message.role === "assistant");
  assert.equal(assistant?.kind === "message" && assistant.message.markdown, "hello");
  const tool = view.items.find((item) => item.kind === "tool")?.tool;
  assert.equal(tool?.status, "success");
  assert.equal(tool?.eventSpan, 1);
  assert.deepEqual(view.integrityIssues, []);
});

test("interrupted stream resumes from replay and converges without duplicated text", () => {
  const partial = [
    projected(0, { type: "turn_started", turn_id: 2 }),
    projected(1, { type: "model_chunk_received", call_id: "m2", delta_text: "part" }),
  ];
  const interrupted = projectConversation(snapshot(partial, "disconnected"));
  assert.equal(interrupted.items[0]?.kind === "message" && interrupted.items[0].message.state, "interrupted");
  assert.equal(interrupted.canResume, true);

  const resumedEvents = [
    ...partial,
    projected(2, { type: "model_chunk_received", call_id: "m2", delta_text: "ial" }),
    projected(3, { type: "assistant_message", text: "partial" }),
    projected(4, { type: "turn_completed", turn_id: 2 }),
  ];
  const resumed = projectConversation(snapshot(resumedEvents));
  const uninterrupted = projectConversation(snapshot(structuredClone(resumedEvents)));
  assert.equal(snapshotConversation(resumed), snapshotConversation(uninterrupted));
  assert.equal(resumed.items[0]?.kind === "message" && resumed.items[0].message.markdown, "partial");
});

test("a closed interrupted draft cannot capture the following turn assistant message", () => {
  const view = projectConversation(snapshot([
    projected(0, { type: "turn_started", turn_id: 1 }),
    projected(1, { type: "model_chunk_received", call_id: "old", delta_text: "old partial" }),
    projected(2, { type: "turn_aborted", turn_id: 1, reason: "cut" }),
    projected(3, { type: "turn_started", turn_id: 2 }),
    projected(4, { type: "assistant_message", text: "new answer" }),
    projected(5, { type: "turn_completed", turn_id: 2 }),
  ]));
  const assistant = view.items.filter((item) => item.kind === "message" && item.message.role === "assistant");
  assert.equal(assistant.length, 2);
  assert.deepEqual(assistant.map((item) => item.kind === "message" && [item.message.markdown, item.message.state]), [
    ["old partial", "interrupted"],
    ["new answer", "complete"],
  ]);
});

test("event-to-component flow renders success, rejection, timeout and cancellation distinctly", () => {
  const events = [
    projected(0, { type: "turn_started", turn_id: 3 }),
    projected(1, { type: "tool_call_requested", call_id: "ok", tool: "read", args: {} }),
    projected(2, { type: "tool_result_added", call_id: "ok", outcome: { success: { value: "done" } } }),
    projected(3, { type: "tool_call_requested", call_id: "deny", tool: "exec", args: {} }),
    projected(4, { type: "tool_result_added", call_id: "deny", outcome: { failure: { error: { code: "approval_rejected", message: "denied" } } } }),
    projected(5, { type: "tool_call_requested", call_id: "slow", tool: "exec", args: {} }),
    projected(6, { type: "tool_execution_terminated", call_id: "slow", termination: "deadline_exceeded" }),
    projected(7, { type: "tool_result_added", call_id: "slow", outcome: { failure: { error: { code: "tool_timeout", message: "late" } } } }),
    projected(8, { type: "tool_call_requested", call_id: "stop", tool: "exec", args: {} }),
    projected(9, { type: "tool_execution_terminated", call_id: "stop", termination: "cancelled" }),
    projected(10, { type: "tool_result_added", call_id: "stop", outcome: { failure: { error: { code: "internal", message: "cancelled" } } } }),
    projected(11, { type: "turn_completed", turn_id: 3 }),
  ];
  const view = projectConversation(snapshot(events));
  assert.deepEqual(view.items.filter((item) => item.kind === "tool").map((item) => item.tool.status), ["success", "rejected", "timed_out", "cancelled"]);
  const markup = renderToStaticMarkup(
    <ChatPanel snapshot={snapshot(events)} onSend={() => undefined} onSteer={() => undefined} onCancel={() => undefined} onResume={() => undefined} />,
  );
  for (const label of ["成功", "已拒绝", "超时", "已取消"]) assert.ok(markup.includes(label));
  assert.ok(markup.includes("approval_rejected"));
  assert.ok(markup.includes("tool_timeout"));
});

test("orphaned tool results fail closed instead of rendering a split result card", () => {
  const view = projectConversation(snapshot([projected(0, { type: "tool_result_added", call_id: "orphan", outcome: { success: { value: "unsafe" } } })]));
  assert.equal(view.items.length, 0);
  assert.equal(view.integrityIssues.length, 1);
  assert.equal(view.canSend, false);
  assert.equal(view.canSteer, false);
});

test("markdown blocks raw HTML, executable protocols and remote images", () => {
  const markdown = '<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>\n\n[bad](javascript:alert(3)) [data](data:text/html,x) ![remote](https://evil.test/x.png) [safe](https://example.com)';
  const markup = renderToStaticMarkup(<SafeMarkdown>{markdown}</SafeMarkdown>);
  assert.ok(!markup.includes("<script"));
  assert.ok(!markup.includes("<img"));
  assert.ok(!markup.includes("javascript:"));
  assert.ok(!markup.includes("data:text"));
  assert.ok(markup.includes("图片已阻止"));
  assert.ok(markup.includes('href="https://example.com"'));
  assert.equal(safeUrlTransform("file:///secret"), "");
  assert.equal(safeUrlTransform("//evil.test/path"), "");
});

test("long markdown is bounded and high-frequency chunks coalesce into one render item", () => {
  const long = prepareMarkdown("x".repeat(100_000));
  assert.equal(long.markdown.length, 65_536);
  assert.equal(long.truncated, true);

  const events = [projected(0, { type: "turn_started", turn_id: 9 })];
  for (let seq = 1; seq <= 20_000; seq += 1) {
    events.push(projected(seq, { type: "model_chunk_received", call_id: "hot", delta_text: "x" }));
  }
  const started = performance.now();
  const view = projectConversation(snapshot(events));
  const elapsed = performance.now() - started;
  assert.equal(view.items.length, 1);
  assert.equal(view.items[0]?.kind === "message" && view.items[0].message.markdown.length, 20_000);
  assert.ok(elapsed < 2_000);
});
