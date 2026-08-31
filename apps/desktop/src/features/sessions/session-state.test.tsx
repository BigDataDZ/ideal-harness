import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import { SessionNavigator } from "./SessionNavigator.tsx";
import { classifyError, operationReducer, snapshotSessionState } from "./session-state.ts";
import type { OperationState, SessionCollectionState, SessionSummary } from "./types.ts";

const session: SessionSummary = {
  sessionId: "demo",
  eventCount: 12,
  generation: 3,
  latestTurnId: 2,
  latestTurnStatus: "completed",
  health: "healthy",
};

test("session state snapshots cover loading, empty, disconnected, corrupt and forbidden", () => {
  const states: SessionCollectionState[] = [
    { kind: "loading" },
    { kind: "ready", sessions: [], selectedId: null },
    { kind: "disconnected", sessions: [session], selectedId: "demo" },
    { kind: "ready", sessions: [{ ...session, health: "corrupt" }], selectedId: null },
    { kind: "forbidden", message: "denied" },
  ];
  assert.deepEqual(states.map(snapshotSessionState), [
    "loading",
    "ready:none:empty",
    "disconnected:demo:demo:healthy:completed",
    "ready:none:demo:corrupt:completed",
    "forbidden",
  ]);
});

test("error interaction routes only by stable code and never by message text", () => {
  assert.deepEqual(classifyError({ code: "cursor_invalid", message: "anything" }), {
    title: "客户端代际已变化",
    action: "refresh",
    tone: "warning",
  });
  assert.equal(classifyError({ code: "internal", message: "cursor_invalid hidden here" }).action, "retry");
  assert.equal(classifyError({ code: "future_code", message: "session_not_found" }).action, "none");
});

test("fork and revert require confirmation and remain pending until projection acknowledgement", () => {
  const fork = { kind: "fork", sourceId: "demo", targetId: "demo-fork", boundary: 12 } as const;
  let state: OperationState = { kind: "idle" };
  state = operationReducer(state, { type: "request", operation: fork });
  assert.equal(state.kind, "confirming");
  state = operationReducer(state, { type: "confirm" });
  assert.equal(state.kind, "submitting");
  state = operationReducer(state, {
    type: "receipt",
    receipt: { sessionId: "demo-fork", eventCount: 12, generation: 4 },
  });
  assert.equal(state.kind, "awaiting_projection");
  state = operationReducer(state, {
    type: "projection_observed",
    sessionId: "demo-fork",
    eventCount: 11,
    generation: 4,
  });
  assert.equal(state.kind, "awaiting_projection");
  state = operationReducer(state, {
    type: "projection_observed",
    sessionId: "demo-fork",
    eventCount: 12,
    generation: 4,
  });
  assert.equal(state.kind, "idle");
});

test("session navigation markup exposes listbox, selection and second-confirm dialog semantics", () => {
  const state: SessionCollectionState = { kind: "ready", sessions: [session], selectedId: "demo" };
  const operation: OperationState = {
    kind: "confirming",
    operation: { kind: "revert", sourceId: "demo", targetId: "demo-revert", turnId: 2 },
  };
  const markup = renderToStaticMarkup(
    <SessionNavigator
      state={state}
      operation={operation}
      onSelect={() => undefined}
      onRequestOperation={() => undefined}
      onConfirmOperation={() => undefined}
      onDismissOperation={() => undefined}
      onRetry={() => undefined}
    />,
  );
  assert.ok(markup.includes('role="listbox"'));
  assert.ok(markup.includes('aria-selected="true"'));
  assert.ok(markup.includes('role="dialog"'));
  assert.ok(markup.includes('aria-modal="true"'));
  assert.ok(markup.includes("二次确认"));
});
