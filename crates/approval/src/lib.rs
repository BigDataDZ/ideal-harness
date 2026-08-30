//! P2-3：fail-closed 审批流。
//! 铁律：审批服务不存在 => 一律拒绝；拒绝时给出的信息要能教育模型正确重试。

mod terminal;

use protocol::{AuthorizationContext, ErrorCode, ErrorEnvelope};
use sandbox_policy::SandboxMode;
use serde::{Deserialize, Serialize};

pub use terminal::TerminalApprover;

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

/// Supplies the security facts that an approval is bound to.
///
/// Implementations must take a fresh snapshot on every call so changes that
/// occur while a human approval is pending can invalidate the decision.
pub trait AuthorizationContextProvider {
    fn current_context(&self) -> Result<AuthorizationContext, ErrorEnvelope>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalGrant {
    pub mode: SandboxMode,
    pub authorization: AuthorizationContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAudit {
    Decided {
        approved: bool,
        authorization: Option<AuthorizationContext>,
    },
    Invalidated {
        previous: AuthorizationContext,
        current: AuthorizationContext,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEvaluation {
    pub result: Result<ApprovalGrant, ErrorEnvelope>,
    pub audits: Vec<ApprovalAudit>,
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

/// fail-closed 提权，并把授权绑定到策略与执行环境事实。
pub fn approve_escalation(
    current: SandboxMode,
    request: EscalationRequest,
    approver: Option<&dyn Approver>,
    context_provider: Option<&dyn AuthorizationContextProvider>,
) -> ApprovalEvaluation {
    if !SandboxPolicyContract::can_widen(current, request.requested_mode) {
        return rejected(
            ErrorCode::SandboxDenied,
            "escalation may only widen, never narrow",
            None,
        );
    }

    let Some(context_provider) = context_provider else {
        return rejected(
            ErrorCode::ApprovalRejected,
            "authorization context is unavailable; failing closed",
            None,
        );
    };
    let before = match context_provider
        .current_context()
        .and_then(validate_context)
    {
        Ok(context) => context,
        Err(error) => return rejected_error(error, None),
    };
    let Some(approver) = approver else {
        return rejected(
            ErrorCode::ApprovalRejected,
            "no approval service is composed; failing closed",
            Some(before),
        );
    };
    if approver.decide(&request) == Decision::Rejected {
        return rejected(
            ErrorCode::ApprovalRejected,
            "escalation rejected by approver; stay within current mode or refine justification",
            Some(before),
        );
    }

    let after = match context_provider
        .current_context()
        .and_then(validate_context)
    {
        Ok(context) => context,
        Err(error) => return rejected_error(error, Some(before)),
    };
    if before != after {
        return ApprovalEvaluation {
            result: Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "authorization context changed while approval was pending; request approval again",
            )),
            audits: vec![
                ApprovalAudit::Decided {
                    approved: false,
                    authorization: Some(before.clone()),
                },
                ApprovalAudit::Invalidated {
                    previous: before,
                    current: after,
                },
            ],
        };
    }

    ApprovalEvaluation {
        result: Ok(ApprovalGrant {
            mode: request.requested_mode,
            authorization: before.clone(),
        }),
        audits: vec![ApprovalAudit::Decided {
            approved: true,
            authorization: Some(before),
        }],
    }
}

/// Revalidate a previously approved grant immediately before its privileged use.
/// A changed context emits an explicit invalidation audit and never reuses the grant.
pub fn validate_grant(
    grant: &ApprovalGrant,
    context_provider: Option<&dyn AuthorizationContextProvider>,
) -> ApprovalEvaluation {
    let Some(context_provider) = context_provider else {
        return ApprovalEvaluation {
            result: Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "authorization context is unavailable; failing closed",
            )),
            audits: Vec::new(),
        };
    };
    let current = match context_provider
        .current_context()
        .and_then(validate_context)
    {
        Ok(context) => context,
        Err(error) => {
            return ApprovalEvaluation {
                result: Err(error),
                audits: Vec::new(),
            };
        }
    };
    if current == grant.authorization {
        return ApprovalEvaluation {
            result: Ok(grant.clone()),
            audits: Vec::new(),
        };
    }
    ApprovalEvaluation {
        result: Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "approved authorization is stale; request approval again",
        )),
        audits: vec![ApprovalAudit::Invalidated {
            previous: grant.authorization.clone(),
            current,
        }],
    }
}

fn validate_context(context: AuthorizationContext) -> Result<AuthorizationContext, ErrorEnvelope> {
    let facts = &context.executor;
    if context.permission_profile_hash.trim().is_empty()
        || facts.os.trim().is_empty()
        || facts.home.trim().is_empty()
        || facts.workspace.trim().is_empty()
    {
        return Err(ErrorEnvelope::new(
            ErrorCode::ApprovalRejected,
            "authorization context contains unknown security facts; failing closed",
        ));
    }
    Ok(context)
}

fn rejected(
    code: ErrorCode,
    message: &'static str,
    authorization: Option<AuthorizationContext>,
) -> ApprovalEvaluation {
    rejected_error(ErrorEnvelope::new(code, message), authorization)
}

fn rejected_error(
    error: ErrorEnvelope,
    authorization: Option<AuthorizationContext>,
) -> ApprovalEvaluation {
    ApprovalEvaluation {
        result: Err(error),
        audits: vec![ApprovalAudit::Decided {
            approved: false,
            authorization,
        }],
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
    use protocol::ExecutorEnvironment;
    use sandbox_policy::SandboxMode::*;
    use std::cell::Cell;

    struct Fixed(Decision);
    impl Approver for Fixed {
        fn decide(&self, _: &EscalationRequest) -> Decision {
            self.0
        }
    }

    struct Contexts {
        values: Vec<AuthorizationContext>,
        next: Cell<usize>,
    }

    impl Contexts {
        fn stable() -> Self {
            let context = context();
            Self {
                values: vec![context.clone(), context],
                next: Cell::new(0),
            }
        }

        fn changed(current: AuthorizationContext) -> Self {
            Self {
                values: vec![context(), current],
                next: Cell::new(0),
            }
        }
    }

    impl AuthorizationContextProvider for Contexts {
        fn current_context(&self) -> Result<AuthorizationContext, ErrorEnvelope> {
            let index = self.next.get();
            self.next.set(index + 1);
            self.values.get(index).cloned().ok_or_else(|| {
                ErrorEnvelope::new(ErrorCode::ApprovalRejected, "context unavailable")
            })
        }
    }

    fn context() -> AuthorizationContext {
        AuthorizationContext {
            policy_epoch: 7,
            permission_profile_hash: "profile-v1".into(),
            executor: ExecutorEnvironment {
                os: "windows".into(),
                home: "C:/Users/test".into(),
                workspace: "D:/work".into(),
                generation: 11,
            },
        }
    }

    fn request(mode: SandboxMode) -> EscalationRequest {
        EscalationRequest {
            requested_mode: mode,
            justification: "need access".into(),
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
        let contexts = Contexts::stable();
        let evaluation =
            approve_escalation(ReadOnly, request(WorkspaceWrite), None, Some(&contexts));
        let e = evaluation.result.unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn rejection_keeps_current_mode() {
        let contexts = Contexts::stable();
        let e = approve_escalation(
            ReadOnly,
            request(DangerFullAccess),
            Some(&Fixed(Decision::Rejected)),
            Some(&contexts),
        )
        .result
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRejected);
    }

    #[test]
    fn narrowing_attempt_rejected_even_with_approver() {
        let e = approve_escalation(
            WorkspaceWrite,
            request(ReadOnly),
            Some(&Fixed(Decision::Approved)),
            Some(&Contexts::stable()),
        )
        .result
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::SandboxDenied);
    }

    #[test]
    fn approved_escalation_returns_bound_grant() {
        let contexts = Contexts::stable();
        let evaluation = approve_escalation(
            ReadOnly,
            request(WorkspaceWrite),
            Some(&Fixed(Decision::Approved)),
            Some(&contexts),
        );
        let grant = evaluation.result.unwrap();
        assert_eq!(grant.mode, WorkspaceWrite);
        assert_eq!(grant.authorization, context());
        assert!(matches!(
            evaluation.audits.as_slice(),
            [ApprovalAudit::Decided { approved: true, .. }]
        ));
    }

    #[test]
    fn missing_or_unknown_context_fails_closed() {
        let missing = approve_escalation(
            ReadOnly,
            request(WorkspaceWrite),
            Some(&Fixed(Decision::Approved)),
            None,
        );
        assert_eq!(
            missing.result.unwrap_err().code,
            ErrorCode::ApprovalRejected
        );

        let mut unknown = context();
        unknown.executor.workspace.clear();
        let contexts = Contexts {
            values: vec![unknown],
            next: Cell::new(0),
        };
        let invalid = approve_escalation(
            ReadOnly,
            request(WorkspaceWrite),
            Some(&Fixed(Decision::Approved)),
            Some(&contexts),
        );
        assert_eq!(
            invalid.result.unwrap_err().code,
            ErrorCode::ApprovalRejected
        );
    }

    #[test]
    fn policy_workspace_and_executor_changes_invalidate_approval() {
        let mut changed_values = Vec::new();

        let mut epoch = context();
        epoch.policy_epoch += 1;
        changed_values.push(epoch);

        let mut profile = context();
        profile.permission_profile_hash = "profile-v2".into();
        profile.executor.workspace = "D:/other".into();
        changed_values.push(profile);

        let mut target = context();
        target.executor.generation += 1;
        changed_values.push(target);

        for changed in changed_values {
            let contexts = Contexts::changed(changed.clone());
            let evaluation = approve_escalation(
                ReadOnly,
                request(WorkspaceWrite),
                Some(&Fixed(Decision::Approved)),
                Some(&contexts),
            );
            assert_eq!(
                evaluation.result.unwrap_err().code,
                ErrorCode::ApprovalRejected
            );
            assert!(matches!(
                evaluation.audits.as_slice(),
                [
                    ApprovalAudit::Decided { approved: false, .. },
                    ApprovalAudit::Invalidated { current, .. }
                ] if current == &changed
            ));
        }
    }

    #[test]
    fn approved_grant_is_revalidated_before_reuse() {
        let contexts = Contexts::stable();
        let grant = approve_escalation(
            ReadOnly,
            request(WorkspaceWrite),
            Some(&Fixed(Decision::Approved)),
            Some(&contexts),
        )
        .result
        .unwrap();

        let mut changed = context();
        changed.executor.generation += 1;
        let changed_provider = Contexts {
            values: vec![changed.clone()],
            next: Cell::new(0),
        };
        let validation = validate_grant(&grant, Some(&changed_provider));
        assert_eq!(
            validation.result.unwrap_err().code,
            ErrorCode::ApprovalRejected
        );
        assert!(matches!(
            validation.audits.as_slice(),
            [ApprovalAudit::Invalidated { current, .. }] if current == &changed
        ));
    }
}
