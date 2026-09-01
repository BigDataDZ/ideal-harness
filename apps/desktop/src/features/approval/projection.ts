/** D8/D13/D16/TASK-907 fail-closed approval validation and event audit projection. */

import type { ProjectionSnapshot, WireEvent } from "../../lib/projection/index.ts";
import type {
  ApprovalAuditRow,
  ApprovalCenterView,
  ApprovalReadiness,
  CurrentSecurityFacts,
  PendingApprovalRequest,
} from "./types.ts";

export function validateApprovalRequest(
  request: PendingApprovalRequest | null,
  current: CurrentSecurityFacts | null,
): ApprovalReadiness {
  if (!request || !current) {
    return { kind: "absent", issues: ["审批服务或请求不在场，默认拒绝"] };
  }
  const invalid: string[] = [];
  if (!safeIdentifier(request.requestId) || !safeIdentifier(request.callId)) invalid.push("请求标识无效");
  if (request.command.trim() === "") invalid.push("实际命令缺失");
  if (request.arguments.some((argument) => /[\u0000-\u001f\u007f]/.test(argument))) invalid.push("命令参数含控制字符");
  if (request.workspace.trim() === "" || request.executor.workspace !== request.workspace) invalid.push("工作区事实不一致");
  if (request.justification.trim() === "") invalid.push("风险原因缺失");
  if (request.permissionProfileHash.trim() === "") invalid.push("权限配置哈希缺失");
  if (request.executor.os.trim() === "" || request.executor.home.trim() === "" || request.executor.generation <= 0) {
    invalid.push("执行环境事实不完整");
  }
  if (request.sandboxMode === "read-only") invalid.push("只读请求不应进入提权审批");
  if (invalid.length > 0) return { kind: "invalid", issues: invalid };

  const stale: string[] = [];
  if (request.desktopGeneration !== current.generation) stale.push("连接 generation 已变化");
  if (request.permissionEpoch !== current.permissionEpoch) stale.push("权限 epoch 已变化");
  if (stale.length > 0) return { kind: "stale", issues: stale };
  return { kind: "ready", issues: [] };
}

export function projectApprovalCenter(
  snapshot: ProjectionSnapshot | null,
  request: PendingApprovalRequest | null,
  security: CurrentSecurityFacts | null,
): ApprovalCenterView {
  const history = snapshot ? projectHistory(snapshot) : [];
  return {
    readiness: validateApprovalRequest(request, security),
    history,
    sourceEvents: snapshot?.events.filter((item) =>
      item.event.type === "approval_decided" || item.event.type === "authorization_invalidated") ?? [],
  };
}

function projectHistory(snapshot: ProjectionSnapshot): ApprovalAuditRow[] {
  const history: ApprovalAuditRow[] = [];
  for (const projected of snapshot.events) {
    const event = projected.event;
    if (event.type === "approval_decided") {
      const authorization = objectField(event, "authorization");
      const executor = objectField(authorization, "executor");
      history.push({
        callId: stringField(event, "call_id") ?? "unknown",
        seq: projected.seq,
        status: event.approved === true ? "approved" : "rejected",
        policyEpoch: integerField(authorization, "policy_epoch"),
        executorGeneration: integerField(executor, "generation"),
        workspace: stringField(executor, "workspace"),
      });
    } else if (event.type === "authorization_invalidated") {
      const current = objectField(event, "current");
      const executor = objectField(current, "executor");
      history.push({
        callId: stringField(event, "call_id") ?? "unknown",
        seq: projected.seq,
        status: "invalidated",
        policyEpoch: integerField(current, "policy_epoch"),
        executorGeneration: integerField(executor, "generation"),
        workspace: stringField(executor, "workspace"),
      });
    }
  }
  return history.reverse();
}

function safeIdentifier(value: string): boolean {
  return value.length > 0 && value.length <= 128 && /^[A-Za-z0-9._:-]+$/.test(value);
}

function objectField(value: Record<string, unknown> | null, field: string): Record<string, unknown> | null {
  const candidate = value?.[field];
  return candidate !== null && typeof candidate === "object" && !Array.isArray(candidate)
    ? (candidate as Record<string, unknown>)
    : null;
}

function stringField(value: Record<string, unknown> | null, field: string): string | null {
  const candidate = value?.[field];
  return typeof candidate === "string" && candidate !== "" ? candidate : null;
}

function integerField(value: Record<string, unknown> | null, field: string): number | null {
  const candidate = value?.[field];
  return typeof candidate === "number" && Number.isSafeInteger(candidate) && candidate >= 0 ? candidate : null;
}
