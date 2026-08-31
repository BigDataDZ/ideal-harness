import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { ProjectionSnapshot } from "../../lib/projection/index.ts";
import { buildTimelineView, snapshotTimelineView } from "./diagnostics.ts";
import { TimelinePanel } from "./TimelinePanel.tsx";

const snapshot: ProjectionSnapshot = {
  sessionId: "demo",
  connectionGeneration: 7,
  connection: "connected",
  lastEventId: "9",
  timelineCursor: 1,
  timelineComplete: true,
  repair: null,
  turns: [
    { turnId: 1, startSeq: 0, endSeq: 9, status: "completed" },
    { turnId: 2, startSeq: 10, endSeq: null, status: "active" },
  ],
  events: [
    { seq: 0, known: true, event: { type: "turn_started", turn_id: 1 } },
    { seq: 1, known: true, event: { type: "token_budget_configured", root_agent_id: "root", token_budget: 1000 } },
    { seq: 2, known: true, event: { type: "token_usage_recorded", usage_id: "u1", agent_path: ["root"], total_tokens: 240, source: "provider" } },
    { seq: 3, known: true, event: { type: "token_usage_recorded", usage_id: "u2", agent_path: ["root"], total_tokens: 60, source: "heuristic" } },
    { seq: 4, known: true, event: { type: "team_member_registered", member: { member_id: "worker", parent_id: "root" } } },
    { seq: 5, known: true, event: { type: "team_task_created", task: { task_id: "t1", owner_member_id: "worker", revision: 1, status: "in_progress", blocked_by: [], write_scopes: ["src"] } } },
    { seq: 6, known: true, event: { type: "team_message_enqueued", message: { message_id: "m1", from_member_id: "root", to_member_id: "worker", body: "work" } } },
    { seq: 7, known: true, event: { type: "team_write_scope_conflict_detected", conflict: { task_id: "t1", conflicting_task_id: "t2", scope: "src" } } },
    { seq: 8, known: true, event: { type: "tool_result_added", call_id: "c1", outcome: { failure: { error: { code: "tool_timeout", message: "deadline" } } } } },
    { seq: 9, known: false, event: { type: "future_diagnostic", payload: "retained" } },
  ],
};

test("timeline diagnostics deterministically derive tokens, team state and stable errors", () => {
  const view = buildTimelineView(snapshot);
  assert.equal(view.tokenUsage, 300);
  assert.equal(view.tokenRemaining, 700);
  assert.deepEqual(view.tokenSources, { provider: 240, heuristic: 60 });
  assert.deepEqual(view.teamMembers, ["worker"]);
  assert.equal(view.teamTasks[0]?.taskId, "t1");
  assert.equal(view.teamMessagesPending, 1);
  assert.equal(view.teamConflicts, 1);
  assert.deepEqual(view.errors.map((error) => [error.code, error.action]), [["tool_timeout", "retry"]]);
  assert.equal(view.rows[0]?.known, false);
  assert.equal(view.rows[0]?.summary, "保留 future_diagnostic，未参与状态归约");
});

test("timeline view snapshot locks the major diagnostics state", () => {
  assert.equal(
    snapshotTimelineView(buildTimelineView(snapshot)),
    '{"session":"demo","connection":"connected","generation":7,"turns":[1,1,0],"tokens":[300,1000,700],"team":[1,1,1,1],"errors":[[8,"tool_timeout","retry"]],"rows":[[9,"future_diagnostic",false],[8,"tool_result_added",true],[7,"team_write_scope_conflict_detected",true],[6,"team_message_enqueued",true],[5,"team_task_created",true],[4,"team_member_registered",true],[3,"token_usage_recorded",true],[2,"token_usage_recorded",true],[1,"token_budget_configured",true],[0,"turn_started",true]]}',
  );
});

test("timeline markup exposes connection, metrics, event list and diagnostic regions", () => {
  const markup = renderToStaticMarkup(<TimelinePanel snapshot={snapshot} />);
  assert.ok(markup.includes("已连接 · gen 7"));
  assert.ok(markup.includes('aria-label="事件时间线"'));
  assert.ok(markup.includes('aria-label="详细诊断"'));
  assert.ok(markup.includes("tool_timeout"));
  assert.ok(markup.includes("未知事件"));
});
