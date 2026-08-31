/** D18/TASK-904 hand-written adapters for the frozen Rust session RPC wire contract. */

export interface WireEvent {
  type: string;
  [field: string]: unknown;
}

export interface SequencedEventDto {
  seq: number;
  event: WireEvent;
}

export interface SessionEventFrameDto {
  session_id: string;
  connection_generation: number;
  record: SequencedEventDto;
}

export type SessionTurnStatusDto = "completed" | "aborted" | "active";

export interface SessionTurnSummaryDto {
  turn_id: number;
  start_seq: number;
  end_seq?: number;
  status: SessionTurnStatusDto;
}

export interface SessionTimelinePageDto {
  session_id: string;
  connection_generation: number;
  turns: SessionTurnSummaryDto[];
  next_cursor?: number;
}

export interface SessionRpcCapabilitiesDto {
  connection_generation: number;
  read_only: boolean;
  timeline: boolean;
  event_stream: boolean;
  last_event_id: boolean;
  follow_before_page: boolean;
  retry_business_errors: boolean;
}

export interface ProjectedEvent {
  seq: number;
  event: WireEvent;
  known: boolean;
}

export interface ProjectedTurn {
  turnId: number;
  startSeq: number;
  endSeq: number | null;
  status: SessionTurnStatusDto;
}

export type RepairReason =
  | "event_gap"
  | "conflicting_duplicate"
  | "generation_changed"
  | "timeline_conflict"
  | "invalid_page";

export interface RepairRequest {
  reason: RepairReason;
  generation: number;
  lastEventId: string | null;
}

export interface ProjectionSnapshot {
  sessionId: string;
  connectionGeneration: number | null;
  events: readonly ProjectedEvent[];
  turns: readonly ProjectedTurn[];
  lastEventId: string | null;
  timelineCursor: number;
  timelineComplete: boolean;
  connection: "idle" | "connected" | "disconnected";
  repair: RepairRequest | null;
}

export type FrameApplyOutcome =
  | "applied"
  | "buffered"
  | "duplicate"
  | "stale_generation"
  | "rebuild_required";

export interface ResumeRequest {
  sessionId: string;
  connectionGeneration: number;
  lastEventId: string | null;
}
