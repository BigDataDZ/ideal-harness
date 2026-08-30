//! P3/TASK-501：MCP 调用接入父事件流、审批审计与现有 spill 存储。

use crate::{AgentLoop, HookContext, HookPoint};
use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use session::SpillStore;
use std::path::{Path, PathBuf};
use tools::{McpCallResult, McpClient, McpRegistry, McpToolHandle};

#[derive(Debug, Clone)]
pub struct McpInvocation {
    call_id: String,
    tool: String,
    arguments: serde_json::Value,
    approved: bool,
    spill_root: PathBuf,
}

impl McpInvocation {
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: serde_json::Value,
        approved: bool,
        spill_root: PathBuf,
    ) -> Result<Self, ErrorEnvelope> {
        let invocation = Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments,
            approved,
            spill_root,
        };
        if invocation.call_id.trim().is_empty()
            || invocation.tool.trim().is_empty()
            || invocation.spill_root.as_os_str().is_empty()
        {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "MCP call id, tool and spill root are required",
            ));
        }
        Ok(invocation)
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }
}

impl AgentLoop<'_> {
    /// 执行一条 MCP 工具调用；无论拒绝或失败都形成完整 call/result 配对。
    pub fn run_mcp_tool(
        &mut self,
        client: &mut McpClient,
        invocation: &McpInvocation,
    ) -> Result<ToolOutcome, ErrorEnvelope> {
        let source = client.source().to_string();
        self.run_mcp_tool_with(&source, invocation, || {
            client.call(&invocation.tool, &invocation.arguments)
        })
    }

    /// 通过受监管 registry 执行；handle 的 generation 会在真正调用前校验。
    pub fn run_registered_mcp_tool(
        &mut self,
        registry: &mut McpRegistry,
        handle: &McpToolHandle,
        invocation: &McpInvocation,
    ) -> Result<ToolOutcome, ErrorEnvelope> {
        let source = handle.source.clone();
        self.run_mcp_tool_with(&source, invocation, || {
            if invocation.tool != handle.name {
                return Err(ErrorEnvelope::new(
                    ErrorCode::ToolArgsInvalid,
                    "MCP invocation tool does not match the managed handle",
                ));
            }
            registry.call(handle, &invocation.arguments)
        })
    }

    fn run_mcp_tool_with<F>(
        &mut self,
        source: &str,
        invocation: &McpInvocation,
        call: F,
    ) -> Result<ToolOutcome, ErrorEnvelope>
    where
        F: FnOnce() -> Result<McpCallResult, ErrorEnvelope>,
    {
        append(
            self.session,
            Event::ToolCallRequested {
                call_id: invocation.call_id.clone(),
                tool: format!("mcp:{source}:{}", invocation.tool),
                args: invocation.arguments.clone(),
            },
        )?;
        let tool_name = format!("mcp:{source}:{}", invocation.tool);
        if let Err(error) = self.execute_hook(HookContext::tool(
            HookPoint::PreToolUse,
            None,
            &invocation.call_id,
            &tool_name,
            None,
        )) {
            append_failure(self.session, &invocation.call_id, &error)?;
            return Err(error);
        }
        append(
            self.session,
            Event::ApprovalDecided {
                call_id: invocation.call_id.clone(),
                approved: invocation.approved,
                authorization: None,
            },
        )?;
        if !invocation.approved {
            let error = ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "MCP invocation rejected by approval policy",
            );
            append_failure(self.session, &invocation.call_id, &error)?;
            return self.finish_failed_mcp_hook(invocation, &tool_name, error);
        }

        let result = match call() {
            Ok(result) => result,
            Err(error) => {
                append_failure(self.session, &invocation.call_id, &error)?;
                return self.finish_failed_mcp_hook(invocation, &tool_name, error);
            }
        };
        let outcome = match project_result(&invocation.call_id, &invocation.spill_root, &result) {
            Ok(outcome) => outcome,
            Err(error) => {
                append_failure(self.session, &invocation.call_id, &error)?;
                return self.finish_failed_mcp_hook(invocation, &tool_name, error);
            }
        };
        append(
            self.session,
            Event::ToolResultAdded {
                call_id: invocation.call_id.clone(),
                outcome: outcome.clone(),
            },
        )?;
        self.execute_hook(HookContext::tool(
            HookPoint::PostToolUse,
            None,
            &invocation.call_id,
            tool_name,
            Some(outcome.clone()),
        ))?;
        Ok(outcome)
    }

    fn finish_failed_mcp_hook(
        &mut self,
        invocation: &McpInvocation,
        tool_name: &str,
        error: ErrorEnvelope,
    ) -> Result<ToolOutcome, ErrorEnvelope> {
        let outcome = ToolOutcome::Failure {
            error: error.clone(),
        };
        match self.execute_hook(HookContext::tool(
            HookPoint::PostToolUse,
            None,
            &invocation.call_id,
            tool_name,
            Some(outcome),
        )) {
            Ok(()) => Err(error),
            Err(hook_error) => Err(hook_error),
        }
    }
}

fn project_result(
    call_id: &str,
    spill_root: &Path,
    result: &McpCallResult,
) -> Result<ToolOutcome, ErrorEnvelope> {
    let output = if result.was_truncated() {
        let preview_chars = result.visible_output().chars().count().max(1);
        let store = SpillStore::create(
            spill_root.to_path_buf(),
            result.output_limit_bytes(),
            preview_chars,
        )
        .map_err(spill_error)?;
        store
            .store(call_id, result.full_output())
            .map_err(spill_error)?
            .event_value()
    } else {
        serde_json::Value::String(result.visible_output().to_string())
    };
    Ok(ToolOutcome::Success {
        value: serde_json::json!({
            "source": result.source(),
            "tool": result.tool(),
            "output": output,
        }),
    })
}

fn append_failure(
    session: &mut dyn session::SessionStore,
    call_id: &str,
    error: &ErrorEnvelope,
) -> Result<(), ErrorEnvelope> {
    append(
        session,
        Event::ToolResultAdded {
            call_id: call_id.to_string(),
            outcome: ToolOutcome::Failure {
                error: error.clone(),
            },
        },
    )
}

fn append(session: &mut dyn session::SessionStore, event: Event) -> Result<(), ErrorEnvelope> {
    session.append(event).map(|_| ()).map_err(|error| {
        ErrorEnvelope::new(
            ErrorCode::Internal,
            format!("failed to append MCP audit event: {error}"),
        )
    })
}

fn spill_error(error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to spill MCP output: {error}"),
    )
}
