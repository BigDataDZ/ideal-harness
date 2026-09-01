export interface ObservedFile {
  path: string;
  content: string | null;
  hash: string | null;
  sourceSeq: number;
  truncated: boolean;
}

export interface FileChange {
  callId: string;
  tool: "fs_write" | "fs_edit";
  path: string;
  requestSeq: number;
  resultSeq: number | null;
  status: "pending" | "applied" | "failed";
  expectedHash: string | null;
  observedHash: string | null;
  casVerified: boolean;
  before: string | null;
  after: string | null;
  errorCode: string | null;
}

export interface WorkspaceView {
  files: readonly ObservedFile[];
  changes: readonly FileChange[];
  integrityIssues: readonly string[];
}

export interface DiffLine {
  kind: "context" | "removed" | "added";
  beforeLine: number | null;
  afterLine: number | null;
  text: string;
}
