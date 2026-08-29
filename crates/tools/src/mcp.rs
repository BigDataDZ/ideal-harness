//! P3/TASK-501：同步 stdio MCP JSON-RPC 客户端；协议异常与进程退出 fail-closed。

use protocol::{ErrorCode, ErrorEnvelope};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub source: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub max_output_bytes: usize,
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), ErrorEnvelope> {
        if self.source.trim().is_empty()
            || self.program.as_os_str().is_empty()
            || self.max_output_bytes == 0
        {
            return Err(args_error(
                "MCP source, program and output limit are required",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub source: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_limit_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallResult {
    source: String,
    tool: String,
    visible_output: String,
    full_output: String,
    output_limit_bytes: usize,
}

impl McpCallResult {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn visible_output(&self) -> &str {
        &self.visible_output
    }

    pub fn full_output(&self) -> &str {
        &self.full_output
    }

    pub fn output_limit_bytes(&self) -> usize {
        self.output_limit_bytes
    }

    pub fn was_truncated(&self) -> bool {
        self.visible_output.len() < self.full_output.len()
    }
}

pub struct McpClient {
    source: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    tools: BTreeMap<String, McpTool>,
    next_id: u64,
    max_output_bytes: usize,
}

impl McpClient {
    pub fn connect(config: McpServerConfig) -> Result<Self, ErrorEnvelope> {
        config.validate()?;
        let mut child = Command::new(&config.program)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| internal(format!("failed to start MCP server: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| internal("MCP server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal("MCP server stdout unavailable"))?;
        let mut client = Self {
            source: config.source,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            tools: BTreeMap::new(),
            next_id: 1,
            max_output_bytes: config.max_output_bytes,
        };
        client.initialize()?;
        client.discover_tools()?;
        Ok(client)
    }

    pub fn tools(&self) -> impl Iterator<Item = &McpTool> {
        self.tools.values()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<McpCallResult, ErrorEnvelope> {
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| args_error(format!("unknown MCP tool: {name}")))?;
        crate::validate_args(
            &crate::ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters_schema: tool.input_schema.clone(),
                escalation_capable: false,
            },
            arguments,
        )?;
        let result = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(internal("MCP tool returned a structured error"));
        }
        let full_output = extract_text_content(&result)?;
        let visible_output = truncate_utf8(&full_output, tool.output_limit_bytes);
        Ok(McpCallResult {
            source: self.source.clone(),
            tool: name.to_string(),
            visible_output,
            full_output,
            output_limit_bytes: tool.output_limit_bytes,
        })
    }

    fn initialize(&mut self) -> Result<(), ErrorEnvelope> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "ideal-harness", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        if result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(internal("MCP initialize result lacks protocolVersion"));
        }
        self.notify("notifications/initialized", serde_json::json!({}))
    }

    fn discover_tools(&mut self) -> Result<(), ErrorEnvelope> {
        let result = self.request("tools/list", serde_json::json!({}))?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| internal("MCP tools/list result lacks tools array"))?;
        let mut discovered = BTreeMap::new();
        for value in tools {
            let name = required_string(value, "name")?;
            if name.trim().is_empty() || discovered.contains_key(name) {
                return Err(internal("MCP returned blank or duplicate tool name"));
            }
            let description = value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input_schema = value
                .get("inputSchema")
                .cloned()
                .ok_or_else(|| internal("MCP tool lacks inputSchema"))?;
            if !input_schema.is_object() {
                return Err(internal("MCP tool inputSchema must be an object"));
            }
            let advertised_limit = value
                .get("outputLimitBytes")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(self.max_output_bytes);
            if advertised_limit == 0 {
                return Err(internal("MCP tool output limit must be greater than zero"));
            }
            discovered.insert(
                name.to_string(),
                McpTool {
                    source: self.source.clone(),
                    name: name.to_string(),
                    description,
                    input_schema,
                    output_limit_bytes: advertised_limit.min(self.max_output_bytes),
                },
            );
        }
        self.tools = discovered;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, ErrorEnvelope> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let response = self.read_message()?;
        if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || response.get("id").and_then(Value::as_u64) != Some(id)
        {
            return Err(internal("MCP response version or id mismatch"));
        }
        if response.get("error").is_some() {
            return Err(internal("MCP server returned a JSON-RPC error"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| internal("MCP response lacks result"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), ErrorEnvelope> {
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn write_message(&mut self, message: &Value) -> Result<(), ErrorEnvelope> {
        serde_json::to_writer(&mut self.stdin, message)
            .map_err(|error| internal(format!("failed to encode MCP request: {error}")))?;
        self.stdin
            .write_all(b"\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| internal(format!("failed to write MCP request: {error}")))
    }

    fn read_message(&mut self) -> Result<Value, ErrorEnvelope> {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .map_err(|error| internal(format!("failed to read MCP response: {error}")))?;
        if read == 0 {
            let status = self.child.try_wait().ok().flatten();
            return Err(internal(format!(
                "MCP server exited before response: {status:?}"
            )));
        }
        serde_json::from_str(&line)
            .map_err(|error| internal(format!("invalid MCP JSON response: {error}")))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ErrorEnvelope> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| internal(format!("MCP value lacks string field {field}")))
}

fn extract_text_content(result: &Value) -> Result<String, ErrorEnvelope> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| internal("MCP tool result lacks content array"))?;
    let mut text = Vec::new();
    for item in content {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            return Err(internal("unsupported MCP content type"));
        }
        text.push(required_string(item, "text")?);
    }
    Ok(text.join("\n"))
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn internal(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, message)
}
