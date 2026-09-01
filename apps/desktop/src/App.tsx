import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useReducer, useState } from "react";

import {
  operationReducer,
  SessionNavigator,
  type CommandErrorDto,
  type SessionCollectionState,
  type SessionOperation,
  type SessionReceiptDto,
} from "./features/sessions/index.ts";
import { ChatPanel } from "./features/chat/index.ts";
import { TimelinePanel } from "./features/timeline/index.ts";
import type { ProjectionSnapshot } from "./lib/projection/index.ts";

interface DesktopStatus {
  operation: string;
  generation: number;
  permissionEpoch: number;
  turnId: number | null;
}

function App() {
  const [security, setSecurity] = useState<DesktopStatus | null>(null);
  const [sessions, setSessions] = useState<SessionCollectionState>({ kind: "loading" });
  const [selectedSnapshot] = useState<ProjectionSnapshot | null>(null);
  const [operation, dispatchOperation] = useReducer(operationReducer, { kind: "idle" });
  const [activeView, setActiveView] = useState<"chat" | "timeline">("chat");

  const connect = useCallback(() => {
    setSessions({ kind: "loading" });
    invoke<DesktopStatus>("desktop_status")
      .then((status) => {
        setSecurity(status);
        setSessions({ kind: "ready", sessions: [], selectedId: null });
      })
      .catch((cause: unknown) => {
        const error = commandError(cause);
        setSecurity(null);
        setSessions(
          error.code === "sandbox_denied"
            ? { kind: "forbidden", message: error.message }
            : { kind: "error", error },
        );
      });
  }, []);

  useEffect(() => connect(), [connect]);

  const submitOperation = useCallback(
    (requested: SessionOperation) => {
      if (!security) {
        dispatchOperation({
          type: "failed",
          error: { code: "internal", message: "Rust 宿主尚未连接" },
        });
        return;
      }
      invoke<SessionReceiptDto>("session_operation", {
        request: operationRequest(requested, security),
      })
        .then((receipt) => dispatchOperation({ type: "receipt", receipt }))
        .catch((cause: unknown) => dispatchOperation({ type: "failed", error: commandError(cause) }));
    },
    [security],
  );

  const requestOperation = (requested: SessionOperation) => {
    dispatchOperation({ type: "request", operation: requested });
    if (requested.kind === "create" || requested.kind === "resume") submitOperation(requested);
  };

  const confirmOperation = () => {
    if (operation.kind !== "confirming") return;
    const requested = operation.operation;
    dispatchOperation({ type: "confirm" });
    submitOperation(requested);
  };

  const selectedId =
    sessions.kind === "ready" || sessions.kind === "disconnected" ? sessions.selectedId : null;

  const turnCommand = (command: "cancel_turn" | "steer_turn", turnId: number, input?: string) => {
    if (!security) return;
    const context = { generation: security.generation, permissionEpoch: security.permissionEpoch };
    void invoke(command, { request: { context, turnId, ...(input === undefined ? {} : { input }) } }).catch(
      () => undefined,
    );
  };

  return (
    <main className="app-shell">
      <SessionNavigator
        state={sessions}
        operation={operation}
        onSelect={(sessionId) => {
          if (sessions.kind === "ready" || sessions.kind === "disconnected") {
            setSessions({ ...sessions, selectedId: sessionId });
          }
        }}
        onRequestOperation={requestOperation}
        onConfirmOperation={confirmOperation}
        onDismissOperation={() => dispatchOperation({ type: "dismiss" })}
        onRetry={connect}
      />
      <div className="workspace-shell">
        <header className="app-header">
          <div className="brand-lockup">
            <span className="brand-mark" aria-hidden="true">IH</span>
            <div><strong>ideal-harness</strong><span>Event-sourced Agent Desktop</span></div>
          </div>
          <div className="host-facts" aria-label="宿主安全状态">
            <span>SESSION {selectedId ?? "NONE"}</span>
            <span>GEN {security?.generation ?? "—"}</span>
            <span>EPOCH {security?.permissionEpoch ?? "—"}</span>
          </div>
        </header>
        <nav className="workspace-tabs" aria-label="会话视图">
          <button type="button" aria-current={activeView === "chat" ? "page" : undefined} onClick={() => setActiveView("chat")}>对话</button>
          <button type="button" aria-current={activeView === "timeline" ? "page" : undefined} onClick={() => setActiveView("timeline")}>Timeline</button>
        </nav>
        {activeView === "chat" ? (
          <ChatPanel
            snapshot={selectedSnapshot}
            startAvailable={false}
            onSend={() => undefined}
            onSteer={(turnId, input) => turnCommand("steer_turn", turnId, input)}
            onCancel={(turnId) => turnCommand("cancel_turn", turnId)}
            onResume={() => {
              if (selectedId) requestOperation({ kind: "resume", sessionId: selectedId });
            }}
          />
        ) : <TimelinePanel snapshot={selectedSnapshot} />}
        <footer className="app-footer">
          <span>TASK-906</span>
          <span>事件是唯一真相源 · 客户端不持久化会话状态</span>
        </footer>
      </div>
    </main>
  );
}

function operationRequest(operation: SessionOperation, status: DesktopStatus): Record<string, unknown> {
  const context = { generation: status.generation, permissionEpoch: status.permissionEpoch };
  switch (operation.kind) {
    case "create":
    case "resume":
      return { operation: operation.kind, context, sessionId: operation.sessionId };
    case "fork":
      return { operation: "fork", context, sourceId: operation.sourceId, targetId: operation.targetId, boundary: operation.boundary };
    case "revert":
      return { operation: "revert", context, sourceId: operation.sourceId, targetId: operation.targetId, turnId: operation.turnId };
  }
}

function commandError(cause: unknown): CommandErrorDto {
  if (cause !== null && typeof cause === "object") {
    const candidate = cause as Record<string, unknown>;
    if (typeof candidate.code === "string" && typeof candidate.message === "string") {
      return { code: candidate.code, message: candidate.message };
    }
  }
  return { code: "internal", message: "宿主暂时不可用" };
}

export default App;
