import type {
  SessionEventFrameDto,
  SessionRpcCapabilitiesDto,
  SessionTimelinePageDto,
  SessionTurnStatusDto,
  SessionTurnSummaryDto,
  WireEvent,
} from "./types.ts";

export class ProjectionProtocolError extends Error {
  readonly code = "invalid_projection_dto";
}

export function parseEventFrame(value: unknown): SessionEventFrameDto {
  const object = record(value, "event frame");
  const sequenced = record(object.record, "sequenced event");
  const event = record(sequenced.event, "event");
  return {
    session_id: nonblank(object.session_id, "session_id"),
    connection_generation: unsigned(object.connection_generation, "connection_generation"),
    record: {
      seq: unsigned(sequenced.seq, "record.seq"),
      event: {
        ...event,
        type: nonblank(event.type, "record.event.type"),
      } as WireEvent,
    },
  };
}

export function parseTimelinePage(value: unknown): SessionTimelinePageDto {
  const object = record(value, "timeline page");
  if (!Array.isArray(object.turns)) {
    throw invalid("turns must be an array");
  }
  const page: SessionTimelinePageDto = {
    session_id: nonblank(object.session_id, "session_id"),
    connection_generation: unsigned(object.connection_generation, "connection_generation"),
    turns: object.turns.map(parseTurn),
  };
  if (object.next_cursor !== undefined) {
    page.next_cursor = unsigned(object.next_cursor, "next_cursor");
  }
  return page;
}

export function parseCapabilities(value: unknown): SessionRpcCapabilitiesDto {
  const object = record(value, "capabilities");
  return {
    connection_generation: unsigned(object.connection_generation, "connection_generation"),
    read_only: boolean(object.read_only, "read_only"),
    timeline: boolean(object.timeline, "timeline"),
    event_stream: boolean(object.event_stream, "event_stream"),
    last_event_id: boolean(object.last_event_id, "last_event_id"),
    follow_before_page: boolean(object.follow_before_page, "follow_before_page"),
    retry_business_errors: boolean(object.retry_business_errors, "retry_business_errors"),
  };
}

function parseTurn(value: unknown): SessionTurnSummaryDto {
  const object = record(value, "turn summary");
  const status = nonblank(object.status, "turn.status");
  if (!(["completed", "aborted", "active"] as string[]).includes(status)) {
    throw invalid("turn.status is unknown");
  }
  const turn: SessionTurnSummaryDto = {
    turn_id: unsigned(object.turn_id, "turn.turn_id"),
    start_seq: unsigned(object.start_seq, "turn.start_seq"),
    status: status as SessionTurnStatusDto,
  };
  if (object.end_seq !== undefined) {
    turn.end_seq = unsigned(object.end_seq, "turn.end_seq");
  }
  return turn;
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw invalid(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function unsigned(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw invalid(`${label} must be a non-negative safe integer`);
  }
  return value;
}

function nonblank(value: unknown, label: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw invalid(`${label} must be a nonblank string`);
  }
  return value;
}

function boolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") {
    throw invalid(`${label} must be boolean`);
  }
  return value;
}

function invalid(message: string): ProjectionProtocolError {
  return new ProjectionProtocolError(message);
}
