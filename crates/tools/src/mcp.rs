//! P3/TASK-501：同步 stdio MCP JSON-RPC 客户端；协议异常与进程退出 fail-closed。

use protocol::{ErrorCode, ErrorEnvelope};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MCP_WIRE_BYTES: usize = 17 * 1024 * 1024;

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
    responses: Receiver<Result<Value, String>>,
    tools: BTreeMap<String, McpTool>,
    next_id: u64,
    max_output_bytes: usize,
    response_timeout: Duration,
}

impl McpClient {
    pub fn connect(config: McpServerConfig) -> Result<Self, ErrorEnvelope> {
        Self::connect_with_timeout(config, DEFAULT_RESPONSE_TIMEOUT)
    }

    pub fn connect_with_timeout(
        config: McpServerConfig,
        response_timeout: Duration,
    ) -> Result<Self, ErrorEnvelope> {
        config.validate()?;
        if response_timeout.is_zero() {
            return Err(args_error("MCP response timeout must be greater than zero"));
        }
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
        let responses = spawn_response_reader(stdout);
        let mut client = Self {
            source: config.source,
            child,
            stdin,
            responses,
            tools: BTreeMap::new(),
            next_id: 1,
            max_output_bytes: config.max_output_bytes,
            response_timeout,
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
        match self.responses.recv_timeout(self.response_timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => {
                let status = self.child.try_wait().ok().flatten();
                Err(internal(format!("{message}; process status: {status:?}")))
            }
            Err(RecvTimeoutError::Timeout) => Err(internal(
                "MCP response exceeded the configured grace period",
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(internal("MCP response reader disconnected"))
            }
        }
    }
}

fn spawn_response_reader(stdout: ChildStdout) -> Receiver<Result<Value, String>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_bounded_line(&mut reader, MAX_MCP_WIRE_BYTES) {
                Ok(None) => {
                    let _ = sender.send(Err("MCP server exited before response".into()));
                    break;
                }
                Ok(Some(line)) => {
                    let response = String::from_utf8(line)
                        .map_err(|error| format!("MCP response is not UTF-8: {error}"))
                        .and_then(|line| {
                            serde_json::from_str(&line)
                                .map_err(|error| format!("invalid MCP JSON response: {error}"))
                        });
                    if sender.send(response).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(format!("failed to read MCP response: {error}")));
                    break;
                }
            }
        }
    });
    receiver
}

fn read_bounded_line<R: BufRead>(reader: &mut R, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP response exceeds the hard wire-size limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn response_reader_enforces_hard_limit_before_unbounded_allocation() {
        let mut allowed = Cursor::new(b"1234\n5678\n".to_vec());
        assert_eq!(
            read_bounded_line(&mut allowed, 5).unwrap().unwrap(),
            b"1234\n"
        );
        assert_eq!(
            read_bounded_line(&mut allowed, 5).unwrap().unwrap(),
            b"5678\n"
        );

        let mut oversized = Cursor::new(b"123456\n".to_vec());
        assert_eq!(
            read_bounded_line(&mut oversized, 5).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
