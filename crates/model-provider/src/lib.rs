//! OpenAI 兼容聊天模型客户端（P1/TASK-102）。
//!
//! 职责边界（所有权地图）：
//! - 阻塞式 HTTP POST `/chat/completions` + SSE 流式解析——同步骨架，不把 tokio 泄漏进公开接口
//! - 纯解析层与 IO 分离：[`parse_sse_line`] / [`extract_delta`] 可脱离网络独立测试
//! - 错误只按结构化字段路由到稳定 [`protocol::ErrorCode`]，禁止解析 message 文本（红线 2）
//!
//! 明确不做：多 provider 抽象层。一个够用，接口留 [`ChatModel`] trait。

mod fetch;

use std::io::{BufRead, Read};
use std::time::Duration;

use protocol::{ErrorCode, ErrorEnvelope, ModelCallSpec, ModelSurfaceMessage, ModelTokenUsage};
use serde::{Deserialize, Serialize};

/// API key 的环境变量名（任务卡指定）。
pub const API_KEY_ENV: &str = "IDEAL_HARNESS_API_KEY";

/// 默认整体请求超时：覆盖连接、发送与整段 SSE 读取。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// 非 2xx 响应体的读取上限：错误体只用于人读，不信任其完整性。
const MAX_ERROR_BODY: u64 = 64 * 1024;

/// 对话消息：OpenAI `messages` 数组的元素。
/// `tool_calls` / `tool_call_id` 仅在工具调用闭环（TASK-103）中使用，
/// 缺省时从线上格式省略，普通对话路径的序列化结果与 TASK-102 完全一致。
///
/// 序列化为手工实现：`tool_calls` 的线上形状是嵌套的
/// `{"id","type":"function","function":{name,arguments}}`（与流式分片的扁平
/// 形状不同），内部统一用扁平的 [`ToolCallRequest`] 承载。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// assistant 发起工具调用时携带（role = "assistant"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    /// role = "tool" 的回填消息携带，对应被应答的调用 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// assistant 消息里 tool_calls 的线上形状（嵌套 function 对象）。
#[derive(Serialize)]
struct ToolCallWire<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    call_type: &'a str,
    function: FunctionWire<'a>,
}

#[derive(Serialize)]
struct FunctionWire<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl Serialize for ChatMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("ChatMessage", 4)?;
        st.serialize_field("role", &self.role)?;
        st.serialize_field("content", &self.content)?;
        if let Some(tcs) = &self.tool_calls {
            let wire: Vec<ToolCallWire<'_>> = tcs
                .iter()
                .map(|t| ToolCallWire {
                    id: &t.id,
                    call_type: "function",
                    function: FunctionWire {
                        name: &t.name,
                        arguments: &t.arguments,
                    },
                })
                .collect();
            st.serialize_field("tool_calls", &wire)?;
        }
        if let Some(id) = &self.tool_call_id {
            st.serialize_field("tool_call_id", id)?;
        }
        st.end()
    }
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// assistant 发起工具调用的消息（content 可为空串）。
    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCallRequest>) -> Self {
        Self {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    /// role = "tool" 的结果回填消息。
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

impl TryFrom<ModelSurfaceMessage> for ChatMessage {
    type Error = serde_json::Error;

    fn try_from(message: ModelSurfaceMessage) -> Result<Self, Self::Error> {
        Ok(match message {
            ModelSurfaceMessage::SystemSummary { text } => {
                ChatMessage::system(format!("Compacted conversation summary:\n{text}"))
            }
            ModelSurfaceMessage::User { text } => ChatMessage::user(text),
            ModelSurfaceMessage::Assistant { text } => ChatMessage::assistant(text),
            ModelSurfaceMessage::AssistantToolCalls { calls, .. } => {
                ChatMessage::assistant_with_tool_calls(
                    calls
                        .into_iter()
                        .map(|call| ToolCallRequest {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                        })
                        .collect(),
                )
            }
            ModelSurfaceMessage::ToolResult { call_id, outcome } => {
                ChatMessage::tool_result(call_id, serde_json::to_string(&outcome)?)
            }
        })
    }
}

/// 一次工具调用请求（流式分片聚合后的最终形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// 上游分配的调用 id（tool 消息回填时必须原样带回）。
    pub id: String,
    /// 函数名。
    pub name: String,
    /// JSON 形式的参数串（流式分片按序拼接）。
    pub arguments: String,
}

/// 单个流式分片中的 tool_calls 增量（OpenAI chunk 形态，按 index 聚合）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ToolCallFragment {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionFragment>,
}

/// 工具调用分片的函数部分。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct FunctionFragment {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// 一条 SSE data 行解析出的结构化增量（纯数据，无 IO）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCallFragment>,
    pub usage: Option<ModelTokenUsage>,
}

/// 一次流式采样的聚合结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatReply {
    /// 全部 delta.content 按到达顺序拼接。
    pub text: String,
    /// 流中最后一次出现的非空 finish_reason（"stop" / "tool_calls" / "length" / ...）。
    pub finish_reason: Option<String>,
    /// 聚合完成的工具调用请求（按分片 index 重组，arguments 已拼接）。
    pub tool_calls: Vec<ToolCallRequest>,
    /// provider 未返回 usage 时为 None，由 agent-loop 启发式兜底。
    pub usage: Option<ModelTokenUsage>,
}

/// SSE 单行三分类结果（纯解析，无 IO）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLine {
    /// 注释行、空行、`event:`/`id:`/`retry:` 行——忽略。
    Ignore,
    /// `data: [DONE]` 哨兵——流正常结束。
    Done,
    /// `data: <payload>`——待结构化解析的 JSON 载荷。
    Data(String),
}

/// 解析一行 SSE 文本：`data:` 前缀取载荷，`[DONE]` 为哨兵，其余一律忽略。
pub fn parse_sse_line(line: &str) -> SseLine {
    let Some(rest) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = rest.trim_start();
    if payload == "[DONE]" {
        SseLine::Done
    } else {
        SseLine::Data(payload.to_string())
    }
}

/// 从一条 data JSON 行提取结构化增量 [`StreamDelta`]。
///
/// 结构化匹配 `choices[0].delta.{content,tool_calls}` 与 `choices[0].finish_reason`；
/// 非 JSON 行返回 [`ErrorCode::ModelStreamBroken`]——静默跳过会掩盖协议漂移，
/// 截断/损坏必须在调用点显式可见（D4/D12）。
pub fn extract_delta(data_line: &str) -> Result<StreamDelta, ErrorEnvelope> {
    #[derive(Deserialize)]
    struct ChunkPayload {
        #[serde(default)]
        choices: Vec<ChunkChoice>,
        usage: Option<ModelTokenUsage>,
    }
    #[derive(Deserialize)]
    struct ChunkChoice {
        #[serde(default)]
        delta: Delta,
        finish_reason: Option<String>,
    }
    #[derive(Deserialize, Default)]
    struct Delta {
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCallFragment>,
    }

    match serde_json::from_str::<ChunkPayload>(data_line) {
        Ok(parsed) => {
            let choice = parsed.choices.first();
            Ok(StreamDelta {
                content: choice.and_then(|c| c.delta.content.clone()),
                finish_reason: choice.and_then(|c| c.finish_reason.clone()),
                tool_calls: choice
                    .map(|c| c.delta.tool_calls.clone())
                    .unwrap_or_default(),
                usage: parsed.usage,
            })
        }
        Err(e) => Err(ErrorEnvelope::new(
            ErrorCode::ModelStreamBroken,
            format!("SSE data 行不符合 chat.completion.chunk 结构: {e}"),
        )),
    }
}

/// 把非 2xx 响应映射为稳定错误码：
/// 结构化匹配 body 中 `error.code == "context_length_exceeded"` →
/// [`ErrorCode::ContextWindowExceeded`]；其余一律 [`ErrorCode::Internal`]。
/// 绝不按 message 或正文文本猜测语义（红线 2）。
fn classify_http_error(status: u16, body: &str) -> ErrorEnvelope {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let code = v
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str());
        if code == Some("context_length_exceeded") {
            return ErrorEnvelope::new(
                ErrorCode::ContextWindowExceeded,
                format!("上游拒绝：上下文超限（HTTP {status}）"),
            );
        }
    }
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("上游 API 返回 HTTP {status}: {}", truncate_for_log(body)),
    )
}

fn truncate_for_log(s: &str) -> String {
    s.chars().take(200).collect()
}

/// 传输层故障映射：超时/流读取中断/解码失败 → [`ErrorCode::ModelStreamBroken`]；
/// 未送达上游（DNS/连接拒绝等）→ [`ErrorCode::Internal`]。
fn map_transport_error(e: reqwest::Error) -> ErrorEnvelope {
    if e.is_timeout() || e.is_decode() {
        ErrorEnvelope::new(ErrorCode::ModelStreamBroken, format!("模型流传输中断: {e}"))
    } else {
        ErrorEnvelope::new(ErrorCode::Internal, format!("请求未送达上游: {e}"))
    }
}

/// agent-loop 依赖的唯一抽象边界（TASK-103 的接入点）。
pub use fetch::{http_fetch_via_proxy, HttpFetchOutcome};

/// agent-loop 依赖的唯一抽象边界（TASK-103 的接入点）。
pub trait ChatModel {
    /// 发起一次流式采样并聚合为完整回复。
    /// `tools` 为 OpenAI tools 数组的原始 JSON（None = 不广告任何工具）。
    fn stream_chat(
        &self,
        spec: &ModelCallSpec,
        messages: &[ChatMessage],
        tools: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope>;
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// OpenAI 兼容客户端（阻塞式）。Clone 安全：内部仅是连接池句柄与密钥副本。
#[derive(Clone)]
pub struct OpenAiCompatClient {
    api_key: String,
    http: reqwest::blocking::Client,
    route: NetworkRoute,
}

impl std::fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatClient")
            .field("api_key", &"[REDACTED]")
            .field("http", &self.http)
            .field("route", &self.route)
            .finish()
    }
}

/// Stable connectivity outcomes for a provider settings check.
/// `provider_message` 携带 Provider 返回的原始错误描述（截断、不含凭据），
/// 用于在 UI 上区分「key 无效」与「余额不足」等真实原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderProbeFailure {
    Authentication { provider_message: Option<String> },
    Network,
    Timeout,
    Rejected { provider_message: Option<String> },
}

#[derive(Clone, Debug)]
enum NetworkRoute {
    LocalProxy,
    LoopbackOnly,
}

impl OpenAiCompatClient {
    /// 从环境变量 [`API_KEY_ENV`] 构造。缺失或为空 → fail-closed 拒绝，
    /// 绝不静默降级为匿名请求。
    pub fn from_env_via_proxy(proxy_url: &str) -> Result<Self, ErrorEnvelope> {
        let key = std::env::var(API_KEY_ENV).map_err(|_| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("环境变量 {API_KEY_ENV} 未设置；拒绝以匿名方式调用上游"),
            )
        })?;
        Self::with_key_via_proxy(key, proxy_url)
    }

    /// 生产构造器：仅接受回环地址上的显式代理，外部 provider 不存在直连路径。
    pub fn with_key_via_proxy(
        api_key: impl Into<String>,
        proxy_url: &str,
    ) -> Result<Self, ErrorEnvelope> {
        Self::with_key_via_proxy_and_timeout(api_key, proxy_url, DEFAULT_TIMEOUT)
    }

    /// 显式超时的代理构造器。
    pub fn with_key_via_proxy_and_timeout(
        api_key: impl Into<String>,
        proxy_url: &str,
        timeout: Duration,
    ) -> Result<Self, ErrorEnvelope> {
        let key = validate_key(api_key.into())?;
        let proxy = reqwest::Url::parse(proxy_url)
            .map_err(|e| ErrorEnvelope::new(ErrorCode::Internal, format!("代理 URL 非法: {e}")))?;
        if proxy.scheme() != "http" || !url_is_loopback(&proxy) {
            return Err(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "模型代理必须是本机回环地址上的 http:// 端点",
            ));
        }
        let configured_proxy = reqwest::Proxy::all(proxy.as_str())
            .map_err(|e| ErrorEnvelope::new(ErrorCode::Internal, format!("代理配置失败: {e}")))?;
        let http = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .proxy(configured_proxy)
            .build()
            .map_err(|e| {
                ErrorEnvelope::new(ErrorCode::Internal, format!("HTTP 客户端初始化失败: {e}"))
            })?;
        Ok(Self {
            api_key: key,
            http,
            route: NetworkRoute::LocalProxy,
        })
    }

    /// 故障注入专用：只允许请求回环地址，不能借此直连外部 provider。
    pub fn with_key_for_loopback_test(
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ErrorEnvelope> {
        let key = validate_key(api_key.into())?;
        let http = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
            .map_err(|e| {
                ErrorEnvelope::new(ErrorCode::Internal, format!("HTTP 客户端初始化失败: {e}"))
            })?;
        Ok(Self {
            api_key: key,
            http,
            route: NetworkRoute::LoopbackOnly,
        })
    }

    /// Checks the provider's authenticated models endpoint without issuing a chat completion.
    pub fn probe(&self, base_url: &str) -> Result<(), ProviderProbeFailure> {
        let base = reqwest::Url::parse(base_url).map_err(|_| ProviderProbeFailure::Rejected { provider_message: None })?;
        match self.route {
            NetworkRoute::LocalProxy if base.scheme() != "https" => {
                return Err(ProviderProbeFailure::Rejected { provider_message: None });
            }
            NetworkRoute::LoopbackOnly if !url_is_loopback(&base) => {
                return Err(ProviderProbeFailure::Rejected { provider_message: None });
            }
            _ => {}
        }
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    ProviderProbeFailure::Timeout
                } else {
                    ProviderProbeFailure::Network
                }
            })?;
        let status = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        let provider_message = extract_provider_message(&body);
        match status {
            200..=299 => Ok(()),
            401 | 403 => Err(ProviderProbeFailure::Authentication { provider_message }),
            _ => Err(ProviderProbeFailure::Rejected { provider_message }),
        }
    }
}

/// 从 Provider 错误响应体提取 `error.message`（OpenAI 兼容形状），
/// 截断至 200 字符；不含任何凭据材料。
fn extract_provider_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value.get("error")?.get("message")?.as_str()?;
    Some(message.chars().take(200).collect())
}

fn validate_key(key: String) -> Result<String, ErrorEnvelope> {
    if key.trim().is_empty() {
        return Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            "API key 为空；拒绝发起无认证请求",
        ));
    }
    Ok(key)
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    url.host_str()
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

impl ChatModel for OpenAiCompatClient {
    fn stream_chat(
        &self,
        spec: &ModelCallSpec,
        messages: &[ChatMessage],
        tools: Option<&serde_json::Value>,
    ) -> Result<ChatReply, ErrorEnvelope> {
        let base = reqwest::Url::parse(&spec.base_url).map_err(|e| {
            ErrorEnvelope::new(ErrorCode::Internal, format!("provider base_url 非法: {e}"))
        })?;
        match self.route {
            NetworkRoute::LocalProxy if base.scheme() != "https" => {
                return Err(ErrorEnvelope::new(
                    ErrorCode::SandboxDenied,
                    "代理模式只允许 HTTPS provider，禁止降级或绕过 CONNECT",
                ));
            }
            NetworkRoute::LoopbackOnly if !url_is_loopback(&base) => {
                return Err(ErrorEnvelope::new(
                    ErrorCode::SandboxDenied,
                    "回环测试客户端拒绝访问外部 provider",
                ));
            }
            _ => {}
        }
        let url = format!("{}/chat/completions", spec.base_url.trim_end_matches('/'));
        let request = ChatRequest {
            model: &spec.model,
            messages,
            stream: true,
            temperature: spec.temperature,
            tools,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&request)
            .send()
            .map_err(map_transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let mut limited = response.take(MAX_ERROR_BODY);
            let mut body = String::new();
            let _ = limited.read_to_string(&mut body);
            return Err(classify_http_error(status.as_u16(), &body));
        }

        consume_sse_stream(response)
    }
}

/// 逐行消费 SSE 响应体并聚合 delta。
///
/// 截断判定：流在 `[DONE]` 哨兵之前结束 → [`ErrorCode::ModelStreamBroken`]，
/// 不把残缺回复当作完整回复交给上层。
fn consume_sse_stream(response: reqwest::blocking::Response) -> Result<ChatReply, ErrorEnvelope> {
    let reader = std::io::BufReader::new(response);
    let mut reply = ChatReply::default();
    // 工具调用分片按 index 重组：id/name 取首个非空值，arguments 按序拼接。
    let mut tool_slots: Vec<Option<ToolCallRequest>> = Vec::new();
    let mut seen_done = false;

    for line in reader.lines() {
        let line = line.map_err(|e| {
            ErrorEnvelope::new(ErrorCode::ModelStreamBroken, format!("SSE 流中断: {e}"))
        })?;
        match parse_sse_line(&line) {
            SseLine::Ignore => {}
            SseLine::Done => {
                seen_done = true;
                break;
            }
            SseLine::Data(payload) => {
                let delta = extract_delta(&payload)?;
                if let Some(t) = delta.content {
                    reply.text.push_str(&t);
                }
                if let Some(f) = delta.finish_reason.filter(|f| !f.is_empty()) {
                    reply.finish_reason = Some(f);
                }
                if delta.usage.is_some() {
                    reply.usage = delta.usage;
                }
                for frag in delta.tool_calls {
                    if tool_slots.len() <= frag.index {
                        tool_slots.resize(frag.index + 1, None);
                    }
                    let slot = tool_slots[frag.index].get_or_insert_with(|| ToolCallRequest {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    if let Some(id) = frag.id {
                        slot.id = id;
                    }
                    if let Some(func) = frag.function {
                        if let Some(name) = func.name {
                            slot.name = name;
                        }
                        if let Some(args) = func.arguments {
                            slot.arguments.push_str(&args);
                        }
                    }
                }
            }
        }
    }

    reply.tool_calls = tool_slots.into_iter().flatten().collect();

    if !seen_done {
        return Err(ErrorEnvelope::new(
            ErrorCode::ModelStreamBroken,
            "SSE 流在 [DONE] 哨兵前结束（疑似截断）",
        ));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-810：SSE 行解析 fuzz——任意字节输入只允许 Ok/Err，绝不 panic。
    #[test]
    fn fuzz_parse_sse_line_never_panics() {
        fn xorshift(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }
        let base = "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}";
        let mut state = 0x1234_5678_u64;
        for _ in 0..3000 {
            let mut bytes = base.as_bytes().to_vec();
            let mutations = (xorshift(&mut state) % 6) + 1;
            for _ in 0..mutations {
                match xorshift(&mut state) % 3 {
                    0 => {
                        let pos = (xorshift(&mut state) as usize) % (bytes.len() + 1);
                        bytes.insert(pos.min(bytes.len()), (xorshift(&mut state) & 0xff) as u8);
                    }
                    1 => {
                        if !bytes.is_empty() {
                            let pos = (xorshift(&mut state) as usize) % bytes.len();
                            bytes.remove(pos);
                        }
                    }
                    _ => {
                        bytes.truncate((xorshift(&mut state) as usize) % (bytes.len() + 1));
                    }
                }
            }
            let line = String::from_utf8_lossy(&bytes).into_owned();
            let _ = parse_sse_line(&line);
        }
    }

    #[test]
    fn parse_sse_line_classifies_all_shapes() {
        assert_eq!(parse_sse_line(""), SseLine::Ignore);
        assert_eq!(parse_sse_line(": keep-alive"), SseLine::Ignore);
        assert_eq!(parse_sse_line("event: message"), SseLine::Ignore);
        assert_eq!(parse_sse_line("id: 42"), SseLine::Ignore);
        assert_eq!(parse_sse_line("dat: x"), SseLine::Ignore);
        assert_eq!(parse_sse_line("data:[DONE]"), SseLine::Done);
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(
            parse_sse_line(r#"data: {"a":1}"#),
            SseLine::Data(r#"{"a":1}"#.into())
        );
    }

    #[test]
    fn extract_delta_reads_content_and_finish_reason() {
        let d = extract_delta(r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(d.content.as_deref(), Some("你好"));
        assert_eq!(d.finish_reason, None);
        assert!(d.tool_calls.is_empty());

        // 收尾块只有 finish_reason
        let d = extract_delta(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).unwrap();
        assert_eq!(d.content, None);
        assert_eq!(d.finish_reason.as_deref(), Some("stop"));

        // 无 choices 的载荷是合法的（部分厂商发心跳帧）
        let d = extract_delta("{}").unwrap();
        assert_eq!(d.content, None);
        assert_eq!(d.finish_reason, None);
        assert!(d.tool_calls.is_empty());
    }

    #[test]
    fn extract_delta_reads_provider_usage_without_choices() {
        let delta = extract_delta(r#"{"choices":[],"usage":{"total_tokens":37}}"#).unwrap();
        assert_eq!(delta.usage, Some(ModelTokenUsage { total_tokens: 37 }));
    }

    #[test]
    fn extract_delta_parses_tool_call_fragments() {
        let d = extract_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1",
                "function":{"name":"echo","arguments":"{\"text\":"}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0].index, 0);
        assert_eq!(d.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            d.tool_calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("echo")
        );
        assert_eq!(
            d.tool_calls[0]
                .function
                .as_ref()
                .unwrap()
                .arguments
                .as_deref(),
            Some(r#"{"text":"#)
        );
    }

    #[test]
    fn non_json_data_line_maps_to_model_stream_broken() {
        let err = extract_delta("<html>bad gateway</html>").unwrap_err();
        assert_eq!(err.code, ErrorCode::ModelStreamBroken);
    }

    #[test]
    fn http_error_body_is_classified_by_structured_code_only() {
        let ctx = classify_http_error(
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"太长了"}}"#,
        );
        assert_eq!(ctx.code, ErrorCode::ContextWindowExceeded);

        // 其余结构化 code / 数字 code / 非 JSON 体 → Internal，绝不猜语义
        for body in [
            r#"{"error":{"code":"invalid_api_key","message":"x"}}"#,
            r#"{"error":{"code":305,"message":"x"}}"#,
            "upstream exploded",
        ] {
            let err = classify_http_error(400, body);
            assert_eq!(err.code, ErrorCode::Internal, "body={body}");
        }
    }

    #[test]
    fn client_rejects_blank_key_fail_closed() {
        let err = OpenAiCompatClient::with_key_via_proxy("   ", "http://127.0.0.1:1").unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("为空"));
    }

    #[test]
    fn production_client_rejects_non_loopback_or_missing_proxy() {
        for proxy in [
            "",
            "https://127.0.0.1:8080",
            "http://localhost:8080",
            "http://proxy.example:8080",
        ] {
            assert!(OpenAiCompatClient::with_key_via_proxy("key", proxy).is_err());
        }
    }

    #[test]
    fn loopback_test_route_rejects_external_provider() {
        let client =
            OpenAiCompatClient::with_key_for_loopback_test("key", DEFAULT_TIMEOUT).unwrap();
        let error = client
            .stream_chat(
                &ModelCallSpec {
                    model: "m".into(),
                    base_url: "https://api.example.com/v1".into(),
                    temperature: None,
                },
                &[ChatMessage::user("hi")],
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::SandboxDenied);
    }

    fn probe_server(status: u16, delay: Duration) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            std::thread::sleep(delay);
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        format!("http://{address}/v1")
    }

    #[test]
    fn provider_probe_redacts_key_and_classifies_stable_outcomes() {
        let ok = probe_server(200, Duration::ZERO);
        let client =
            OpenAiCompatClient::with_key_for_loopback_test("key", Duration::from_secs(2)).unwrap();
        assert_eq!(client.probe(&ok), Ok(()));
        let debug = format!("{client:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("api_key: \"key\""));

        let unauthorized = probe_server(401, Duration::ZERO);
        assert_eq!(
            client.probe(&unauthorized),
            Err(ProviderProbeFailure::Authentication)
        );

        let slow = probe_server(200, Duration::from_millis(150));
        let short =
            OpenAiCompatClient::with_key_for_loopback_test("key", Duration::from_millis(20))
                .unwrap();
        assert_eq!(short.probe(&slow), Err(ProviderProbeFailure::Timeout));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable = format!("http://{}/v1", listener.local_addr().unwrap());
        let closer = std::thread::spawn(move || drop(listener.accept().unwrap()));
        assert_eq!(
            client.probe(&unavailable),
            Err(ProviderProbeFailure::Network)
        );
        closer.join().unwrap();
    }

    #[test]
    fn assistant_tool_calls_serialize_to_nested_wire_shape() {
        let msg = ChatMessage::assistant_with_tool_calls(vec![ToolCallRequest {
            id: "call_1".into(),
            name: "now".into(),
            arguments: "{}".into(),
        }]);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""type":"function""#),
            "tool_calls 线上形状必须含 type 字段: {json}"
        );
        assert!(
            json.contains(r#""function":{"name":"now","arguments":"{}"}"#),
            "function 必须是嵌套对象: {json}"
        );

        let tool = ChatMessage::tool_result("call_1", r#"{"ok":true}"#);
        let jt = serde_json::to_string(&tool).unwrap();
        assert!(jt.contains(r#""tool_call_id":"call_1""#), "{jt}");
    }

    #[test]
    fn model_surface_conversion_preserves_tool_arguments_and_outcome() {
        let assistant = ChatMessage::try_from(ModelSurfaceMessage::AssistantToolCalls {
            request_id: "r1".into(),
            calls: vec![protocol::ModelToolCall {
                id: "c1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":1}"#.into(),
            }],
        })
        .unwrap();
        assert_eq!(assistant.tool_calls.unwrap()[0].arguments, r#"{"q":1}"#);

        let outcome = protocol::ToolOutcome::Success {
            value: serde_json::json!({"answer": 42}),
        };
        let tool = ChatMessage::try_from(ModelSurfaceMessage::ToolResult {
            call_id: "c1".into(),
            outcome: outcome.clone(),
        })
        .unwrap();
        assert_eq!(tool.content, serde_json::to_string(&outcome).unwrap());
    }
}
