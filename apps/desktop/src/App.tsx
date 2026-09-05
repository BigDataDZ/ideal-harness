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
import { ApprovalCenter, type PendingApprovalRequest } from "./features/approval/index.ts";
import { ChatPanel } from "./features/chat/index.ts";
import { TimelinePanel } from "./features/timeline/index.ts";
import { WorkspacePanel } from "./features/workspace/index.ts";
import {
  SettingsPanel,
  type ProbeResult,
  type ProviderSettings,
  type ProviderSettingsSnapshot,
} from "./features/settings/index.tsx";
import { SessionProjection, type ProjectionSnapshot } from "./lib/projection/index.ts";

interface DesktopStatus {
  operation: string;
  generation: number;
  permissionEpoch: number;
  turnId: number | null;
}

function App() {
  const [security, setSecurity] = useState<DesktopStatus | null>(null);
  const [sessions, setSessions] = useState<SessionCollectionState>({ kind: "loading" });
  const [selectedSnapshot, setSelectedSnapshot] = useState<ProjectionSnapshot | null>(null);
  const [operation, dispatchOperation] = useReducer(operationReducer, { kind: "idle" });
  const [activeView, setActiveView] = useState<"chat" | "timeline" | "approval" | "workspace" | "settings">("chat");
  const [pendingApproval] = useState<PendingApprovalRequest | null>(null);
  const [providerSettings, setProviderSettings] = useState<ProviderSettingsSnapshot | null>(null);
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [settingsMessage, setSettingsMessage] = useState<string | null>(null);

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

  // TASK-909：以 SessionProjection 从宿主事件帧重建选中会话的唯一快照。
  const loadSnapshot = useCallback(async (sessionId: string) => {
    const projection = new SessionProjection(sessionId);
    let lastSeq = 0;
    for (;;) {
      const frames = await invoke<
        Array<{ session_id: string; connection_generation: number; record: { seq: number; event: unknown } }>
      >("session_event_frames", { request: { sessionId, lastSeq, limit: 500 } });
      if (frames.length === 0) break;
      for (const frame of frames) projection.applyFrame(frame);
      lastSeq = frames[frames.length - 1].record.seq + 1;
      if (frames.length < 500) break;
    }
    setSelectedSnapshot(projection.snapshot());
  }, []);

  const loadSettings = useCallback(() => {
    invoke<ProviderSettingsSnapshot>("get_provider_settings")
      .then(setProviderSettings)
      .catch((cause: unknown) => setSettingsMessage(commandError(cause).message));
  }, []);

  useEffect(() => loadSettings(), [loadSettings]);

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
        .then((receipt) => {
          dispatchOperation({ type: "receipt", receipt });
          void loadSnapshot(receipt.sessionId);
        })
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

  const decideApproval = (request: PendingApprovalRequest, approved: boolean): Promise<void> => {
    if (!security) return Promise.reject({ code: "approval_rejected", message: "审批服务不在场" });
    return invoke("respond_approval", {
      request: {
        context: { generation: security.generation, permissionEpoch: security.permissionEpoch },
        requestId: request.requestId,
        executorGeneration: request.executor.generation,
        approved,
      },
    }).then(() => undefined);
  };

  const settingsContext = () => {
    if (!security) throw new Error("host unavailable");
    return { generation: security.generation, permissionEpoch: security.permissionEpoch };
  };

  const settingsMutation = async (command: string, details: Record<string, unknown>) => {
    setSettingsBusy(true);
    setSettingsMessage(null);
    try {
      const receipt = await invoke<DesktopStatus>(command, {
        request: { context: settingsContext(), ...details },
      });
      setSecurity(receipt);
      loadSettings();
      setSettingsMessage("设置已生效，安全代际已更新");
    } catch (cause) {
      const error = commandError(cause);
      setSettingsMessage(error.message);
      if (error.code === "cursor_invalid") {
        // 代际失步：刷新宿主状态后让用户重试，而不是停留在必败状态
        connect();
      }
    } finally {
      setSettingsBusy(false);
    }
  };

  const saveSettings = (settings: ProviderSettings) =>
    settingsMutation("save_provider_settings", { settings });
  const storeKey = (settings: ProviderSettings, apiKey: string) =>
    settingsMutation("store_api_key", { settings, apiKey });
  const deleteKey = () =>
    settingsMutation("delete_api_key", {});
  const probeProvider = async (): Promise<ProbeResult> => {
    setSettingsBusy(true);
    try {
      // TASK-909 修复：响应为 externally-tagged 枚举，归一化为 { kind, providerMessage }
      const raw = await invoke<Record<string, { provider_message?: string } | null>>(
        "test_provider_connection",
        { request: { context: settingsContext() } },
      );
      const kind = Object.keys(raw)[0] as ProbeResult;
      const providerMessage = raw[kind]?.provider_message ?? null;
      const labels: Record<ProbeResult, string> = {
        connected: "连接成功",
        authentication_failed: "认证失败：请更新 API Key",
        network_unavailable: "网络不可达",
        timed_out: "连接超时",
        rejected: "Provider 拒绝请求",
      };
      const label = `${labels[kind] ?? "Provider 拒绝请求"}${providerMessage ? `（${providerMessage}）` : ""}`;
      setSettingsMessage(label);
      return kind;
    } catch (cause) {
      setSettingsMessage(commandError(cause).message);
      return "rejected";
    } finally {
      setSettingsBusy(false);
    }
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
          <button type="button" aria-current={activeView === "approval" ? "page" : undefined} onClick={() => setActiveView("approval")}>审批</button>
          <button type="button" aria-current={activeView === "workspace" ? "page" : undefined} onClick={() => setActiveView("workspace")}>工作区</button>
          <button type="button" aria-current={activeView === "settings" ? "page" : undefined} onClick={() => setActiveView("settings")}>设置</button>
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
        ) : activeView === "timeline" ? (
          <TimelinePanel snapshot={selectedSnapshot} />
        ) : activeView === "approval" ? (
          <ApprovalCenter
            snapshot={selectedSnapshot}
            request={pendingApproval}
            security={security ? { generation: security.generation, permissionEpoch: security.permissionEpoch } : null}
            onDecision={decideApproval}
          />
        ) : activeView === "workspace" ? <WorkspacePanel snapshot={selectedSnapshot} /> : (
          <SettingsPanel
            snapshot={providerSettings}
            busy={settingsBusy}
            message={settingsMessage}
            onSave={saveSettings}
            onStoreKey={storeKey}
            onDeleteKey={deleteKey}
            onProbe={probeProvider}
          />
        )}
        <footer className="app-footer">
          <span>TASK-908</span>
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
