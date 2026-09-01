import type { ProjectedEvent } from "../../lib/projection/index.ts";

export type SandboxMode = "read-only" | "workspace-write" | "danger-full-access";

export interface ApprovalExecutorFacts {
  os: string;
  home: string;
  workspace: string;
  generation: number;
}

export interface PendingApprovalRequest {
  requestId: string;
  callId: string;
  command: string;
  arguments: readonly string[];
  workspace: string;
  sandboxMode: SandboxMode;
  justification: string;
  desktopGeneration: number;
  permissionEpoch: number;
  permissionProfileHash: string;
  executor: ApprovalExecutorFacts;
}

export interface CurrentSecurityFacts {
  generation: number;
  permissionEpoch: number;
}

export type ApprovalReadiness =
  | { kind: "absent"; issues: readonly string[] }
  | { kind: "invalid" | "stale"; issues: readonly string[] }
  | { kind: "ready"; issues: readonly [] };

export interface ApprovalAuditRow {
  callId: string;
  seq: number;
  status: "approved" | "rejected" | "invalidated";
  policyEpoch: number | null;
  executorGeneration: number | null;
  workspace: string | null;
}

export interface ApprovalCenterView {
  readiness: ApprovalReadiness;
  history: readonly ApprovalAuditRow[];
  sourceEvents: readonly ProjectedEvent[];
}
