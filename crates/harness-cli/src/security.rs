//! P2/TASK-206：CLI 安全链路装配（D5/D7/D8/D9）。

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;

use approval::{approve_escalation, validate_escalation_args, Approver, EscalationRequest};
use network_proxy::{ProxyPolicy, ProxyServer};
use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use sandbox_exec::{CommandSpec, RestrictedBackend, RestrictedProcessPool};
use sandbox_policy::SandboxMode;
use tools::{ToolAudit, ToolExecution, ToolRegistry, ToolSpec};

pub(crate) struct ProviderProxy {
    pub(crate) url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<std::io::Result<()>>>,
}

impl ProviderProxy {
    pub(crate) fn start(base_url: &str, events: Arc<Mutex<Vec<Event>>>) -> anyhow::Result<Self> {
        let provider = reqwest::Url::parse(base_url)?;
        let host = provider
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("provider base_url 缺少主机名"))?;
        let policy = ProxyPolicy::for_provider(host).map_err(anyhow::Error::msg)?;
        let server = ProxyServer::bind(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            policy,
            move |event| {
                events
                    .lock()
                    .map_err(|_| std::io::Error::other("proxy audit queue poisoned"))?
                    .push(event);
                Ok(())
            },
        )?;
        let address = server.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || server.serve_until(&worker_stop));
        Ok(Self {
            url: format!("http://{address}"),
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("network proxy thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for ProviderProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn register_exec_tool<B>(
    registry: &mut ToolRegistry,
    backend: B,
    approver: Option<Arc<dyn Approver + Send + Sync>>,
) where
    B: RestrictedBackend + Send + Sync + 'static,
{
    let pool = RestrictedProcessPool::new(backend);
    registry.register_audited(
        ToolSpec {
            name: "exec".into(),
            description: "在 OS 受限子进程中执行外部命令".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["program"],
                "properties": {
                    "program": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } }
                }
            }),
            escalation_capable: true,
        },
        Box::new(move |args| execute_restricted(args, &pool, approver.as_deref())),
    );
}

fn execute_restricted<B: RestrictedBackend>(
    args: &serde_json::Value,
    pool: &RestrictedProcessPool<B>,
    approver: Option<&(dyn Approver + Send + Sync)>,
) -> ToolExecution {
    let requested = args
        .get("sandbox_permissions")
        .cloned()
        .map(serde_json::from_value::<SandboxMode>)
        .transpose();
    let requested = match requested {
        Ok(mode) => mode,
        Err(error) => {
            return ToolExecution::new(ToolOutcome::Failure {
                error: ErrorEnvelope::new(
                    ErrorCode::ApprovalRejected,
                    format!("invalid sandbox_permissions: {error}"),
                ),
            });
        }
    };
    let justification = args.get("justification").and_then(|value| value.as_str());
    if let Err(error) = validate_escalation_args(requested, justification) {
        return ToolExecution::new(ToolOutcome::Failure { error });
    }

    let mut audits = Vec::new();
    if let Some(requested_mode) = requested {
        let request = EscalationRequest {
            requested_mode,
            justification: justification.unwrap_or_default().to_string(),
        };
        let decision = approve_escalation(
            SandboxMode::ReadOnly,
            request,
            approver.map(|value| value as &dyn Approver),
        );
        audits.push(ToolAudit::ApprovalDecided {
            approved: decision.is_ok(),
        });
        if let Err(error) = decision {
            return ToolExecution {
                outcome: ToolOutcome::Failure { error },
                audits,
            };
        }
    }

    let mut command = CommandSpec::new(args["program"].as_str().unwrap_or_default());
    if let Some(command_args) = args.get("args").and_then(|value| value.as_array()) {
        for arg in command_args.iter().filter_map(|value| value.as_str()) {
            command = command.arg(arg);
        }
    }
    let outcome = match pool.execute(&command) {
        Ok(output) if output.restricted && output.process_id != std::process::id() => {
            ToolOutcome::Success {
                value: serde_json::json!({
                    "process_id": output.process_id,
                    "exit_code": output.exit_code,
                    "restricted": output.restricted,
                    "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                }),
            }
        }
        Ok(_) => ToolOutcome::Failure {
            error: ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "restricted backend did not prove child-process isolation",
            ),
        },
        Err(error) => ToolOutcome::Failure {
            error: ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                format!("restricted process execution failed: {error}"),
            ),
        },
    };
    ToolExecution { outcome, audits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_tools_json;
    use approval::Decision;
    use model_provider::{ChatMessage, ChatModel, OpenAiCompatClient};
    use protocol::ModelCallSpec;
    use sandbox_exec::{ExecutionOutput, PlatformRestrictedBackend};
    use std::io;
    use std::time::Duration;
    use tools::EscalationAvailability;

    #[derive(Clone, Copy)]
    struct FakeRestrictedBackend;

    impl RestrictedBackend for FakeRestrictedBackend {
        fn execute(&self, _: &CommandSpec) -> io::Result<ExecutionOutput> {
            Ok(ExecutionOutput {
                process_id: std::process::id() + 1,
                exit_code: 0,
                stdout: b"sandboxed".to_vec(),
                stderr: Vec::new(),
                restricted: true,
            })
        }
    }

    struct FixedApprover(Decision);

    impl Approver for FixedApprover {
        fn decide(&self, _: &EscalationRequest) -> Decision {
            self.0
        }
    }

    fn escalation_args() -> serde_json::Value {
        serde_json::json!({
            "program": "ignored.exe",
            "sandbox_permissions": "workspace-write",
            "justification": "need to write generated files"
        })
    }

    #[test]
    fn exec_escalation_without_approver_fails_closed_and_is_audited() {
        let mut registry = ToolRegistry::default();
        registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
        register_exec_tool(&mut registry, FakeRestrictedBackend, None);
        let run = registry
            .dispatch_with_audit("exec", &escalation_args())
            .unwrap();
        assert_eq!(run.audits, [ToolAudit::ApprovalDecided { approved: false }]);
        match run.outcome {
            ToolOutcome::Failure { error } => assert_eq!(error.code, ErrorCode::ApprovalRejected),
            other => panic!("expected approval failure, got {other:?}"),
        }
    }

    #[test]
    fn approved_exec_returns_proven_restricted_child_result() {
        let mut registry = ToolRegistry::default();
        registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
        register_exec_tool(
            &mut registry,
            FakeRestrictedBackend,
            Some(Arc::new(FixedApprover(Decision::Approved))),
        );
        let run = registry
            .dispatch_with_audit("exec", &escalation_args())
            .unwrap();
        assert_eq!(run.audits, [ToolAudit::ApprovalDecided { approved: true }]);
        match run.outcome {
            ToolOutcome::Success { value } => {
                assert_eq!(value["restricted"], true);
                assert_ne!(value["process_id"], std::process::id());
            }
            other => panic!("expected restricted success, got {other:?}"),
        }
    }

    #[test]
    fn restricted_exec_advertises_paired_escalation_fields() {
        let mut registry = ToolRegistry::default();
        registry.set_escalation_availability(EscalationAvailability::RestrictedBackendMounted);
        register_exec_tool(&mut registry, FakeRestrictedBackend, None);
        let tools = openai_tools_json(&registry, &["exec"]).unwrap();
        let properties = &tools[0]["function"]["parameters"]["properties"];
        assert!(properties.get("sandbox_permissions").is_some());
        assert!(properties.get("justification").is_some());
    }

    #[cfg(windows)]
    #[test]
    fn exec_tool_uses_real_windows_restricted_process_pool() {
        let mut registry = ToolRegistry::default();
        register_exec_tool(&mut registry, PlatformRestrictedBackend, None);
        let run = registry
            .dispatch_with_audit(
                "exec",
                &serde_json::json!({
                    "program": "cmd.exe",
                    "args": ["/D", "/C", "echo p2-integrated"]
                }),
            )
            .unwrap();
        match run.outcome {
            ToolOutcome::Success { value } => {
                assert_eq!(value["restricted"], true);
                assert_ne!(value["process_id"], std::process::id());
                assert!(value["stdout"].as_str().unwrap().contains("p2-integrated"));
            }
            other => panic!("expected real restricted execution, got {other:?}"),
        }
    }

    #[test]
    fn model_request_through_deny_all_proxy_is_rejected_and_audited() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let server = ProxyServer::bind(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            ProxyPolicy::deny_all(),
            move |event| {
                captured.lock().unwrap().push(event);
                Ok(())
            },
        )
        .unwrap();
        let proxy_url = format!("http://{}", server.local_addr().unwrap());
        let worker = thread::spawn(move || server.serve_once());
        let client = OpenAiCompatClient::with_key_via_proxy_and_timeout(
            "test-key",
            &proxy_url,
            Duration::from_secs(2),
        )
        .unwrap();
        let result = client.stream_chat(
            &ModelCallSpec {
                model: "m".into(),
                base_url: "https://blocked.example/v1".into(),
                temperature: None,
            },
            &[ChatMessage::user("hi")],
            None,
        );
        assert!(result.is_err());
        worker.join().unwrap().unwrap();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            [Event::NetworkAccessDenied {
                host: "blocked.example".into(),
                port: 443,
                reason: "host_not_allowlisted".into(),
            }]
        );
    }
}
