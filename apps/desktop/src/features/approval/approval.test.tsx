import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { ProjectedEvent, ProjectionSnapshot } from "../../lib/projection/index.ts";
import { ApprovalCenter } from "./ApprovalCenter.tsx";
import { projectApprovalCenter, validateApprovalRequest } from "./projection.ts";
import type { PendingApprovalRequest } from "./types.ts";

const security = { generation: 4, permissionEpoch: 9 };
const request: PendingApprovalRequest = {
  requestId: "approval-1",
  callId: "call-1",
  command: "cargo",
  arguments: ["test", "--workspace"],
  workspace: "D:/work",
  sandboxMode: "workspace-write",
  justification: "测试需要写入 target 缓存",
  desktopGeneration: 4,
  permissionEpoch: 9,
  permissionProfileHash: "profile-a",
  executor: { os: "windows", home: "C:/Users/test", workspace: "D:/work", generation: 7 },
};

test("approval absence, incomplete facts and stale epochs all fail closed", () => {
  assert.equal(validateApprovalRequest(null, security).kind, "absent");
  assert.equal(validateApprovalRequest(request, null).kind, "absent");
  assert.equal(validateApprovalRequest({ ...request, workspace: "D:/other" }, security).kind, "invalid");
  assert.equal(validateApprovalRequest({ ...request, permissionEpoch: 8 }, security).kind, "stale");
  assert.equal(validateApprovalRequest({ ...request, desktopGeneration: 3 }, security).kind, "stale");
  assert.equal(validateApprovalRequest({ ...request, justification: "" }, security).kind, "invalid");
  assert.equal(validateApprovalRequest(request, security).kind, "ready");
});

test("approval center exposes every actual security fact and only a one-time approval", () => {
  const markup = renderToStaticMarkup(
    <ApprovalCenter snapshot={null} request={request} security={security} onDecision={() => Promise.resolve()} />,
  );
  for (const fact of ["cargo", "test", "--workspace", "D:/work", "profile-a", "windows · generation 7", "C:/Users/test", "call-1"]) {
    assert.ok(markup.includes(fact));
  }
  assert.ok(markup.includes("仅批准本次"));
  assert.ok(!markup.includes("永久批准"));
  assert.ok(/仅批准本次<\/button>/.test(markup));
  assert.ok(markup.includes("disabled"));
});

test("approval audit is derived from decision and invalidation events", () => {
  const events = [
    projected(1, { type: "approval_decided", call_id: "call-1", approved: true, authorization: authorization(9, 7) }),
    projected(2, { type: "authorization_invalidated", call_id: "call-1", previous: authorization(9, 7), current: authorization(10, 8) }),
  ];
  const view = projectApprovalCenter(snapshot(events), null, security);
  assert.deepEqual(view.history.map((row) => [row.status, row.seq, row.policyEpoch, row.executorGeneration]), [
    ["invalidated", 2, 10, 8],
    ["approved", 1, 9, 7],
  ]);
  assert.equal(view.sourceEvents.length, 2);
});

function authorization(policyEpoch: number, generation: number) {
  return {
    policy_epoch: policyEpoch,
    permission_profile_hash: "profile-a",
    executor: { os: "windows", home: "C:/Users/test", workspace: "D:/work", generation },
  };
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
