import { parseCapabilities, parseEventFrame, parseTimelinePage } from "./adapters.ts";
import type {
  FrameApplyOutcome,
  ProjectedEvent,
  ProjectedTurn,
  ProjectionSnapshot,
  RepairReason,
  ResumeRequest,
  SessionEventFrameDto,
  SessionTimelinePageDto,
  SessionTurnSummaryDto,
  WireEvent,
} from "./types.ts";

const KNOWN_EVENTS = new Set([
  "turn_started",
  "user_message",
  "user_input_queued",
  "assistant_message",
  "model_chunk_received",
  "tool_call_requested",
  "tool_result_added",
  "model_tool_calls_requested",
  "compaction_applied",
  "token_budget_configured",
  "token_usage_recorded",
  "approval_decided",
  "authorization_invalidated",
  "network_access_denied",
  "subagent_started",
  "subagent_cancellation_requested",
  "subagent_report_delivered",
  "subagent_stopped",
  "tool_execution_terminated",
  "memory_recorded",
  "memory_revoked",
  "memory_context_injected",
  "team_member_registered",
  "team_message_enqueued",
  "team_message_delivered",
  "team_task_created",
  "team_task_updated",
  "team_write_scope_conflict_detected",
  "turn_completed",
  "turn_aborted",
]);

export class SessionProjection {
  readonly sessionId: string;

  private generation: number | null = null;
  private connection: ProjectionSnapshot["connection"] = "idle";
  private readonly events = new Map<number, ProjectedEvent>();
  private readonly pendingEvents = new Map<number, SessionEventFrameDto>();
  private readonly eventTurns = new Map<number, ProjectedTurn>();
  private readonly timelinePages = new Map<number, SessionTimelinePageDto>();
  private timelineTurns: ProjectedTurn[] = [];
  private timelineCursor = 0;
  private timelineComplete = false;
  private watermark: number | null = null;
  private repairReason: RepairReason | null = null;

  constructor(sessionId: string) {
    if (!/^[A-Za-z0-9_-]+$/.test(sessionId)) {
      throw new Error("session id contains unsupported characters");
    }
    this.sessionId = sessionId;
  }

  connect(capabilitiesInput: unknown): void {
    const capabilities = parseCapabilities(capabilitiesInput);
    if (
      !capabilities.read_only ||
      !capabilities.timeline ||
      !capabilities.event_stream ||
      !capabilities.last_event_id ||
      !capabilities.follow_before_page ||
      capabilities.retry_business_errors
    ) {
      throw new Error("server capabilities violate the read-only projection contract");
    }
    this.acceptGeneration(capabilities.connection_generation);
    this.connection = "connected";
  }

  disconnect(): void {
    this.connection = "disconnected";
  }

  applyFrame(input: unknown): FrameApplyOutcome {
    const frame = parseEventFrame(input);
    this.requireSession(frame.session_id);
    const generation = this.acceptGeneration(frame.connection_generation);
    if (generation === "stale") {
      return "stale_generation";
    }
    const seq = frame.record.seq;
    const existing = this.events.get(seq);
    if (existing) {
      if (stableJson(existing.event) === stableJson(frame.record.event)) {
        return "duplicate";
      }
      this.requireRepair("conflicting_duplicate");
      return "rebuild_required";
    }
    const pending = this.pendingEvents.get(seq);
    if (pending) {
      if (stableJson(pending.record.event) === stableJson(frame.record.event)) {
        return "duplicate";
      }
      this.requireRepair("conflicting_duplicate");
      return "rebuild_required";
    }

    const expected = this.watermark === null ? 0 : this.watermark + 1;
    if (seq > expected) {
      this.pendingEvents.set(seq, frame);
      this.requireRepair("event_gap");
      return "buffered";
    }
    if (seq < expected) {
      this.requireRepair("conflicting_duplicate");
      return "rebuild_required";
    }
    this.commitFrame(frame);
    this.drainPending();
    return "applied";
  }

  applyTimelinePage(requestCursor: number, input: unknown): void {
    if (!Number.isSafeInteger(requestCursor) || requestCursor < 0) {
      throw new Error("timeline request cursor is invalid");
    }
    const page = parseTimelinePage(input);
    this.requireSession(page.session_id);
    if (this.acceptGeneration(page.connection_generation) === "stale") {
      return;
    }
    const next = requestCursor + page.turns.length;
    if (page.next_cursor !== undefined && page.next_cursor !== next) {
      this.requireRepair("invalid_page");
      return;
    }
    const existing = this.timelinePages.get(requestCursor);
    if (existing && stableJson(existing) !== stableJson(page)) {
      this.requireRepair("timeline_conflict");
      return;
    }
    this.timelinePages.set(requestCursor, page);
    this.materializeTimeline();
  }

  replaceFromReplay(frames: readonly unknown[], timelinePages: readonly [number, unknown][]): void {
    const generation = this.generation;
    this.resetAuthority();
    this.generation = generation;
    for (const [cursor, page] of timelinePages) {
      this.applyTimelinePage(cursor, page);
    }
    for (const frame of frames) {
      const outcome = this.applyFrame(frame);
      if (outcome !== "applied" && outcome !== "duplicate") {
        throw new Error("replay is not contiguous and authoritative");
      }
    }
    if (this.pendingEvents.size !== 0) {
      throw new Error("replay ended with an event gap");
    }
    this.repairReason = null;
  }

  resumeRequest(): ResumeRequest | null {
    if (this.generation === null) {
      return null;
    }
    return {
      sessionId: this.sessionId,
      connectionGeneration: this.generation,
      lastEventId: this.watermark === null ? null : String(this.watermark),
    };
  }

  snapshot(): ProjectionSnapshot {
    const mergedTurns = new Map<number, ProjectedTurn>();
    for (const turn of this.timelineTurns) {
      mergedTurns.set(turn.turnId, { ...turn });
    }
    for (const turn of this.eventTurns.values()) {
      const timeline = mergedTurns.get(turn.turnId);
      if (
        !timeline ||
        timeline.status === "active" ||
        (turn.endSeq !== null && (timeline.endSeq === null || turn.endSeq >= timeline.endSeq))
      ) {
        mergedTurns.set(turn.turnId, { ...turn });
      }
    }
    const turns = [...mergedTurns.values()].sort(
      (left, right) => left.startSeq - right.startSeq || left.turnId - right.turnId,
    );
    return {
      sessionId: this.sessionId,
      connectionGeneration: this.generation,
      events: [...this.events.values()].map((event) => ({
        seq: event.seq,
        known: event.known,
        event: structuredClone(event.event),
      })),
      turns,
      lastEventId: this.watermark === null ? null : String(this.watermark),
      timelineCursor: this.timelineCursor,
      timelineComplete: this.timelineComplete,
      connection: this.connection,
      repair:
        this.repairReason === null || this.generation === null
          ? null
          : {
              reason: this.repairReason,
              generation: this.generation,
              lastEventId: this.watermark === null ? null : String(this.watermark),
            },
    };
  }

  private acceptGeneration(incoming: number): "accepted" | "stale" {
    if (this.generation === null) {
      this.generation = incoming;
      return "accepted";
    }
    if (incoming === this.generation) {
      return "accepted";
    }
    if (incoming < this.generation) {
      return "stale";
    }
    this.resetAuthority();
    this.generation = incoming;
    this.requireRepair("generation_changed");
    return "accepted";
  }

  private resetAuthority(): void {
    this.events.clear();
    this.pendingEvents.clear();
    this.eventTurns.clear();
    this.timelinePages.clear();
    this.timelineTurns = [];
    this.timelineCursor = 0;
    this.timelineComplete = false;
    this.watermark = null;
    this.repairReason = null;
  }

  private commitFrame(frame: SessionEventFrameDto): void {
    const projected: ProjectedEvent = {
      seq: frame.record.seq,
      event: structuredClone(frame.record.event),
      known: KNOWN_EVENTS.has(frame.record.event.type),
    };
    this.events.set(projected.seq, projected);
    this.watermark = projected.seq;
    if (projected.known) {
      this.reduceKnownEvent(projected.seq, projected.event);
    }
  }

  private drainPending(): void {
    while (this.watermark !== null) {
      const next = this.watermark + 1;
      const frame = this.pendingEvents.get(next);
      if (!frame) {
        break;
      }
      this.pendingEvents.delete(next);
      this.commitFrame(frame);
    }
    if (this.pendingEvents.size === 0 && this.repairReason === "event_gap") {
      this.repairReason = null;
    }
  }

  private reduceKnownEvent(seq: number, event: WireEvent): void {
    if (event.type === "turn_started") {
      const turnId = integerField(event, "turn_id");
      if (turnId !== null) {
        this.eventTurns.set(turnId, {
          turnId,
          startSeq: seq,
          endSeq: null,
          status: "active",
        });
      }
      return;
    }
    if (event.type !== "turn_completed" && event.type !== "turn_aborted") {
      return;
    }
    const turnId = integerField(event, "turn_id");
    const turn = turnId === null ? undefined : this.eventTurns.get(turnId);
    if (turn) {
      this.eventTurns.set(turn.turnId, {
        ...turn,
        endSeq: seq,
        status: event.type === "turn_completed" ? "completed" : "aborted",
      });
    }
  }

  private materializeTimeline(): void {
    const turns: ProjectedTurn[] = [];
    const seen = new Set<number>();
    let cursor = 0;
    let complete = false;
    while (true) {
      const page = this.timelinePages.get(cursor);
      if (!page) {
        break;
      }
      for (const turn of page.turns) {
        if (seen.has(turn.turn_id)) {
          this.requireRepair("timeline_conflict");
          return;
        }
        seen.add(turn.turn_id);
        turns.push(projectTurn(turn));
      }
      if (page.next_cursor === undefined) {
        cursor += page.turns.length;
        complete = true;
        break;
      }
      if (page.next_cursor <= cursor) {
        this.requireRepair("invalid_page");
        return;
      }
      cursor = page.next_cursor;
    }
    this.timelineTurns = turns;
    this.timelineCursor = cursor;
    this.timelineComplete = complete;
  }

  private requireSession(sessionId: string): void {
    if (sessionId !== this.sessionId) {
      throw new Error("projection frame belongs to another session");
    }
  }

  private requireRepair(reason: RepairReason): void {
    this.repairReason = reason;
  }
}

function projectTurn(turn: SessionTurnSummaryDto): ProjectedTurn {
  return {
    turnId: turn.turn_id,
    startSeq: turn.start_seq,
    endSeq: turn.end_seq ?? null,
    status: turn.status,
  };
}

function integerField(event: WireEvent, field: string): number | null {
  const value = event[field];
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableJson).join(",")}]`;
  }
  if (value !== null && typeof value === "object") {
    const object = value as Record<string, unknown>;
    return `{${Object.keys(object)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableJson(object[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}
