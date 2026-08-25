//! P2-3：fail-closed 审批流。
//! 铁律：审批服务不存在 => 一律拒绝；拒绝时给出的信息要能教育模型正确重试。

use protocol::{ErrorCode, ErrorEnvelope};
use sandbox_policy::SandboxMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Rejected,
}

/// 审批器抽象：生产实现接人工 UI / 策略引擎 / 远程审批服务。
pub trait Approver {
    fn decide(&self, request: &EscalationRequest) -> Decision;
}

/// 一次提权请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationRequest {
    pub requested_mode: SandboxMode,
    /// 必须是说明"为什么需要加宽权限"的非空句子。
    pub justification: String,
}

/// 提权参数成对校验（P2-4 的 schema 广告前提）：
/// 裸提权、孤儿理由、空白理由都是非法组合。
pub fn validate_escalation_args(
    requested: Option<SandboxMode>,
    justification: Option<&str>,
) -> Result<(), ErrorEnvelope> {
    match (requested, justification) {
        (None, None) => Ok(()),
        (Some(_), None) => Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "escalation requires a justification",
        )),
        (None, Some(_)) => Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "justification is only valid together with sandbox_permissions",
        )),
        (Some(_), Some(j)) if j.trim().is_empty() => Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "justification must be a non-empty sentence",
        )),
        (Some(_), Some(_)) => Ok(()),
    }
}

/// fail-closed 提权：
/// 1) 收窄方向直接拒绝；2) 无审批器拒绝；3) 审批器拒绝则保持原模式。
pub fn approve_escalation(
    current: SandboxMode,
    request: EscalationRequest,
    approver: Option<&dyn Approver>,
) -> Result<SandboxMode, ErrorEnvelope> {
    if !SandboxPolicyContract::can_widen(current, request.requested_mode) {
        return Err(ErrorEnvelope::new(
            ErrorCode::SandboxDenied,
            "escalation may only widen, never narrow",
        ));
    }
    match approver {
        None => Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "no approval service is composed; failing closed",
        )),
        Some(a) => match a.decide(&request) {
            Decision::Approved => Ok(request.requested_mode),
            Decision::Rejected => Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "escalation rejected by approver; stay within current mode or refine justification",
            )),
        },
    }
}

/// 对 sandbox-policy 的薄封装，避免 approval 直接依赖 policy 内部 API 形态。
struct SandboxPolicyContract;

impl SandboxPolicyContract {
    fn can_widen(from: SandboxMode, to: SandboxMode) -> bool {
        sandbox_policy::SandboxPolicy::can_widen(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox_policy::SandboxMode::*;

    struct Fixed(Decision);
    impl Approver for Fixed {
        fn decide(&self, _: &EscalationRequest) -> Decision {
            self.0
        }
    }

    #[test]
    fn orphan_justification_is_invalid() {
        let e = validate_escalation_args(None, Some("because")).unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn bare_permissions_without_justification_is_invalid() {
        let e = validate_escalation_args(Some(WorkspaceWrite), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn blank_justification_is_invalid() {
        assert!(validate_escalation_args(Some(WorkspaceWrite), Some("   ")).is_err());
    }

    #[test]
    fn valid_pair_passes_and_noop_passes() {
        assert!(validate_escalation_args(
            Some(WorkspaceWrite),
            Some("need to write workspace files")
        )
        .is_ok());
        assert!(validate_escalation_args(None, None).is_ok());
    }

    #[test]
    fn no_approver_fails_closed() {
        let e = approve_escalation(
            ReadOnly,
            EscalationRequest {
                requested_mode: WorkspaceWrite,
                justification: "need".into(),
            },
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn rejection_keeps_current_mode() {
        let e = approve_escalation(
            ReadOnly,
            EscalationRequest {
                requested_mode: DangerFullAccess,
                justification: "need".into(),
            },
            Some(&Fixed(Decision::Rejected)),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn narrowing_attempt_rejected_even_with_approver() {
        let e = approve_escalation(
            WorkspaceWrite,
            EscalationRequest {
                requested_mode: ReadOnly,
                justification: "narrow".into(),
            },
            Some(&Fixed(Decision::Approved)),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::SandboxDenied);
    }

    #[test]
    fn approved_escalation_returns_new_mode() {
        let m = approve_escalation(
            ReadOnly,
            EscalationRequest {
                requested_mode: WorkspaceWrite,
                justification: "need write".into(),
            },
            Some(&Fixed(Decision::Approved)),
        )
        .unwrap();
        assert_eq!(m, WorkspaceWrite);
    }
}
