/** D13/D17/TASK-906 event-derived chat view types. */

import type { StableErrorCode } from "../sessions/index.ts";

export interface ChatMessage {
  id: string;
  seq: number;
  turnId: number | null;
  role: "user" | "assistant" | "system";
  markdown: string;
  state: "complete" | "streaming" | "interrupted" | "queued";
  callId: string | null;
}

export interface ToolErrorView {
  code: StableErrorCode | "unknown";
  message: string;
}

export interface ToolCardView {
  callId: string;
  turnId: number | null;
  tool: string;
  args: unknown;
  requestedSeq: number;
  resultSeq: number | null;
  eventSpan: number | null;
  status: "running" | "success" | "rejected" | "failed" | "timed_out" | "cancelled";
  resultPreview: string | null;
  error: ToolErrorView | null;
  audit: readonly string[];
}

export interface ConversationItemMessage {
  kind: "message";
  seq: number;
  message: ChatMessage;
}

export interface ConversationItemTool {
  kind: "tool";
  seq: number;
  tool: ToolCardView;
}

export type ConversationItem = ConversationItemMessage | ConversationItemTool;

export interface ConversationView {
  sessionId: string;
  connection: "idle" | "connected" | "disconnected";
  activeTurnId: number | null;
  items: readonly ConversationItem[];
  integrityIssues: readonly string[];
  canSend: boolean;
  canSteer: boolean;
  canCancel: boolean;
  canResume: boolean;
}
