import assert from "node:assert/strict";
import test from "node:test";

import { parseSseBlock, SessionProjection } from "./index.ts";

const capabilities = (generation: number) => ({
  connection_generation: generation,
  read_only: true,
  timeline: true,
  event_stream: true,
  last_event_id: true,
  follow_before_page: true,
  retry_business_errors: false,
});

const frame = (generation: number, seq: number, event: Record<string, unknown>) => ({
  session_id: "demo",
  connection_generation: generation,
  record: { seq, event },
});

const page = (
  generation: number,
  turns: Record<string, unknown>[],
  nextCursor?: number,
) => ({
  session_id: "demo",
  connection_generation: generation,
  turns,
  ...(nextCursor === undefined ? {} : { next_cursor: nextCursor }),
});

test("out-of-order frames request gap repair, drain deterministically, and deduplicate", () => {
  const projection = new SessionProjection("demo");
  projection.connect(capabilities(4));
  const second = frame(4, 1, { type: "user_message", text: "hello" });
  assert.equal(projection.applyFrame(second), "buffered");
  assert.equal(projection.snapshot().repair?.reason, "event_gap");
  assert.equal(projection.snapshot().lastEventId, null);

  const first = frame(4, 0, { type: "turn_started", turn_id: 7 });
  assert.equal(projection.applyFrame(first), "applied");
  assert.equal(projection.snapshot().events.length, 2);
  assert.equal(projection.snapshot().lastEventId, "1");
  assert.equal(projection.snapshot().repair, null);
  assert.equal(projection.applyFrame(second), "duplicate");
  assert.equal(projection.snapshot().events.length, 2);

  assert.equal(
    projection.applyFrame(frame(4, 1, { type: "user_message", text: "changed" })),
    "rebuild_required",
  );
  assert.equal(projection.snapshot().repair?.reason, "conflicting_duplicate");
});

test("disconnect resumes with Last-Event-ID and restart rejects old generation", () => {
  const projection = new SessionProjection("demo");
  projection.connect(capabilities(2));
  projection.applyFrame(frame(2, 0, { type: "turn_started", turn_id: 1 }));
  projection.disconnect();
  assert.deepEqual(projection.resumeRequest(), {
    sessionId: "demo",
    connectionGeneration: 2,
    lastEventId: "0",
  });
  projection.connect(capabilities(3));
  assert.equal(projection.snapshot().events.length, 0);
  assert.equal(projection.snapshot().repair?.reason, "generation_changed");
  assert.equal(
    projection.applyFrame(frame(2, 1, { type: "user_message", text: "stale" })),
    "stale_generation",
  );
  assert.equal(projection.applyFrame(frame(3, 0, { type: "turn_started", turn_id: 2 })), "applied");
});

test("concurrent timeline pages materialize only from the contiguous pagination watermark", () => {
  const projection = new SessionProjection("demo");
  projection.connect(capabilities(8));
  projection.applyTimelinePage(
    1,
    page(8, [{ turn_id: 2, start_seq: 3, end_seq: 5, status: "completed" }]),
  );
  assert.equal(projection.snapshot().turns.length, 0);
  projection.applyTimelinePage(
    0,
    page(8, [{ turn_id: 1, start_seq: 0, end_seq: 2, status: "completed" }], 1),
  );
  assert.deepEqual(
    projection.snapshot().turns.map((turn) => turn.turnId),
    [1, 2],
  );
  assert.equal(projection.snapshot().timelineCursor, 2);
  assert.equal(projection.snapshot().timelineComplete, true);
  projection.applyFrame(frame(8, 0, { type: "turn_started", turn_id: 1 }));
  assert.equal(projection.snapshot().turns[0]?.status, "completed");
});

test("unknown events remain visible without silently mutating derived state", () => {
  const projection = new SessionProjection("demo");
  projection.connect(capabilities(1));
  projection.applyFrame(frame(1, 0, { type: "future_event", turn_id: 99, payload: "kept" }));
  const snapshot = projection.snapshot();
  assert.equal(snapshot.events[0]?.known, false);
  assert.equal(snapshot.events[0]?.event.payload, "kept");
  assert.deepEqual(snapshot.turns, []);
});

test("gap repair replay converges to the same view as an uninterrupted event replay", () => {
  const frames = [
    frame(6, 0, { type: "turn_started", turn_id: 9 }),
    frame(6, 1, { type: "user_message", text: "hello" }),
    frame(6, 2, { type: "assistant_message", text: "world" }),
    frame(6, 3, { type: "turn_completed", turn_id: 9 }),
  ];
  const timeline = [
    0,
    page(6, [{ turn_id: 9, start_seq: 0, end_seq: 3, status: "completed" }]),
  ] as const;

  const uninterrupted = new SessionProjection("demo");
  uninterrupted.connect(capabilities(6));
  uninterrupted.applyTimelinePage(timeline[0], timeline[1]);
  for (const item of frames) uninterrupted.applyFrame(item);

  const repaired = new SessionProjection("demo");
  repaired.connect(capabilities(6));
  repaired.applyFrame(frames[2]);
  repaired.replaceFromReplay(frames, [[timeline[0], timeline[1]]]);
  assert.deepEqual(repaired.snapshot(), uninterrupted.snapshot());
});

test("SSE adapter binds id to record.seq and rejects malformed blocks", () => {
  const payload = JSON.stringify(frame(3, 5, { type: "assistant_message", text: "ok" }));
  assert.equal(
    parseSseBlock(`id: 5\nevent: session_event\ndata: ${payload}\n\n`).record.seq,
    5,
  );
  assert.throws(() => parseSseBlock(`id: 4\nevent: session_event\ndata: ${payload}\n\n`));
  assert.throws(() => parseSseBlock("event: session_event\ndata: {}\n\n"));
});
