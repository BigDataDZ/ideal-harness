//! P3/TASK-501：MCP 调用接入父事件流、审批审计与现有 spill 存储。

use crate::AgentLoop;
use protocol::{ErrorCode, ErrorEnvelope, Event, ToolOutcome};
use session::SpillStore;
use std::path::{Path, PathBuf};
use tools::{McpCallResult, McpClient};

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
        append(
            self.session,
            Event::ToolCallRequested {
                call_id: invocation.call_id.clone(),
                tool: format!("mcp:{}:{}", client.source(), invocation.tool),
                args: invocation.arguments.clone(),
            },
        )?;
        append(
            self.session,
            Event::ApprovalDecided {
                call_id: invocation.call_id.clone(),
                approved: invocation.approved,
            },
        )?;
        if !invocation.approved {
            let error = ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "MCP invocation rejected by approval policy",
            );
            append_failure(self.session, &invocation.call_id, &error)?;
            return Err(error);
        }

        let result = match client.call(&invocation.tool, &invocation.arguments) {
            Ok(result) => result,
            Err(error) => {
                append_failure(self.session, &invocation.call_id, &error)?;
                return Err(error);
            }
        };
        let outcome = match project_result(&invocation.call_id, &invocation.spill_root, &result) {
            Ok(outcome) => outcome,
            Err(error) => {
                append_failure(self.session, &invocation.call_id, &error)?;
                return Err(error);
            }
        };
        append(
            self.session,
            Event::ToolResultAdded {
                call_id: invocation.call_id.clone(),
                outcome: outcome.clone(),
            },
        )?;
        Ok(outcome)
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
