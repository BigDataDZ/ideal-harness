//! P5/TASK-504/TASK-605：只读 HTTP RPC、generation 与无窗口 SSE 补洞。

use protocol::{
    ErrorCode, ErrorEnvelope, RpcErrorResponse, SessionEventFrame, SessionEventQuery,
    SessionRpcCapabilities, SessionTimelinePage, SessionTimelineQuery, SessionTurnStatus,
    SessionTurnSummary,
};
use session::{replay_session, timeline_page, TurnStatus};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod http;
use http::{handle_connection, HttpResponse};

const DEFAULT_BIND: &str = "127.0.0.1:8765";
const DEFAULT_TIMELINE_LIMIT: u32 = 20;
const MAX_TIMELINE_LIMIT: u32 = 1_000;
static NEXT_CONNECTION_GENERATION: AtomicU64 = AtomicU64::new(1);

struct ServeArgs {
    root: PathBuf,
    bind: SocketAddr,
}

pub(crate) fn cmd_serve(args: &[String]) -> anyhow::Result<()> {
    let config = parse_args(args)?;
    let server = ReadOnlySessionServer::bind(&config.root, config.bind)?;
    println!(
        "只读会话服务监听 http://{}（root={}）",
        server.local_addr()?,
        config.root.display()
    );
    server.run()
}

fn parse_args(args: &[String]) -> anyhow::Result<ServeArgs> {
    let mut root = None;
    let mut bind = DEFAULT_BIND.parse::<SocketAddr>()?;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let value = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("{flag} 缺少取值"))?;
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--bind" => {
                bind = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--bind 必须是 IP:端口"))?
            }
            other => anyhow::bail!("未知 serve 参数：{other}"),
        }
        index += 2;
    }
    Ok(ServeArgs {
        root: root.ok_or_else(|| anyhow::anyhow!("serve 缺少 --root <session-dir>"))?,
        bind,
    })
}

struct ReadOnlySessionServer {
    listener: TcpListener,
    root: PathBuf,
    generation: u64,
}

impl ReadOnlySessionServer {
    fn bind(root: &Path, address: SocketAddr) -> anyhow::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(anyhow::anyhow!(
                ErrorEnvelope::new(
                    ErrorCode::SandboxDenied,
                    format!("read-only session server refuses non-loopback bind: {address}"),
                )
                .message
            ));
        }
        if !root.is_dir() {
            anyhow::bail!(
                "session root does not exist or is not a directory: {}",
                root.display()
            );
        }
        let generation = NEXT_CONNECTION_GENERATION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("read-only RPC generation overflow"))?;
        Ok(Self {
            listener: TcpListener::bind(address)?,
            root: root.to_path_buf(),
            generation,
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn run(self) -> anyhow::Result<()> {
        for stream in self.listener.incoming() {
            handle_connection(
                stream?,
                &self.root,
                |root, method, target, last_event_id| {
                    route_with_generation(root, method, target, last_event_id, self.generation)
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
fn route(root: &Path, method: &str, target: &str) -> HttpResponse {
    route_with_generation(root, method, target, None, 1)
}

fn route_with_generation(
    root: &Path,
    method: &str,
    target: &str,
    last_event_id: Option<&str>,
    generation: u64,
) -> HttpResponse {
    if method != "GET" {
        return error_response(
            405,
            ErrorEnvelope::new(ErrorCode::SandboxDenied, "read-only RPC accepts GET only"),
        );
    }
    match parse_target(target, last_event_id, generation) {
        Ok(Route::Capabilities) => capabilities_response(generation),
        Ok(Route::Timeline(query)) => timeline_response(root, query, generation),
        Ok(Route::Events(query)) => event_response(root, query, generation),
        Err(error) => error_response(400, error),
    }
}

enum Route {
    Capabilities,
    Timeline(SessionTimelineQuery),
    Events(SessionEventQuery),
}

fn parse_target(
    target: &str,
    last_event_id: Option<&str>,
    generation: u64,
) -> Result<Route, ErrorEnvelope> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path.trim_matches('/') == "v1/capabilities" {
        if !query.is_empty() || last_event_id.is_some() {
            return Err(cursor_error("capabilities route does not accept cursors"));
        }
        return Ok(Route::Capabilities);
    }
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts.len() != 4 || parts[0] != "v1" || parts[1] != "sessions" {
        return Err(cursor_error("unknown read-only RPC route"));
    }
    let session_id = validate_session_id(parts[2])?;
    match parts[3] {
        "timeline" => {
            if last_event_id.is_some() {
                return Err(cursor_error(
                    "Last-Event-ID is only valid for event streams",
                ));
            }
            let values = parse_query(query, &["cursor", "limit", "generation"])?;
            let connection_generation =
                parse_optional_u64(values.get("generation").copied(), "generation")?;
            validate_generation(connection_generation, generation)?;
            let cursor = parse_optional_u64(values.get("cursor").copied(), "cursor")?;
            let limit = parse_optional_u64(values.get("limit").copied(), "limit")?
                .unwrap_or(u64::from(DEFAULT_TIMELINE_LIMIT));
            let limit = u32::try_from(limit).map_err(|_| cursor_error("limit is too large"))?;
            if limit == 0 || limit > MAX_TIMELINE_LIMIT {
                return Err(cursor_error(format!(
                    "limit must be between 1 and {MAX_TIMELINE_LIMIT}"
                )));
            }
            Ok(Route::Timeline(SessionTimelineQuery {
                session_id,
                connection_generation,
                cursor,
                limit,
            }))
        }
        "events" => {
            let values = parse_query(query, &["last_seq", "generation"])?;
            let connection_generation =
                parse_optional_u64(values.get("generation").copied(), "generation")?;
            validate_generation(connection_generation, generation)?;
            if values.contains_key("last_seq") && last_event_id.is_some() {
                return Err(cursor_error(
                    "last_seq and Last-Event-ID cannot be supplied together",
                ));
            }
            let last_seq = match last_event_id {
                Some(value) => parse_optional_u64(Some(value), "Last-Event-ID")?,
                None => parse_optional_u64(values.get("last_seq").copied(), "last_seq")?,
            };
            Ok(Route::Events(SessionEventQuery {
                session_id,
                connection_generation,
                last_seq,
            }))
        }
        _ => Err(cursor_error("unknown read-only RPC route")),
    }
}

fn validate_generation(requested: Option<u64>, current: u64) -> Result<(), ErrorEnvelope> {
    if requested.is_some_and(|generation| generation != current) {
        return Err(cursor_error("connection generation is stale"));
    }
    Ok(())
}

fn parse_query<'a>(
    query: &'a str,
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<&'a str, &'a str>, ErrorEnvelope> {
    let mut values = std::collections::BTreeMap::new();
    if query.is_empty() {
        return Ok(values);
    }
    for pair in query.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| cursor_error("query parameters require key=value"))?;
        if !allowed.contains(&key) || value.is_empty() || values.insert(key, value).is_some() {
            return Err(cursor_error(format!(
                "invalid or duplicate query parameter: {key}"
            )));
        }
    }
    Ok(values)
}

fn parse_optional_u64(value: Option<&str>, name: &str) -> Result<Option<u64>, ErrorEnvelope> {
    value
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| cursor_error(format!("{name} must be a non-negative integer")))
        })
        .transpose()
}

fn validate_session_id(value: &str) -> Result<String, ErrorEnvelope> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(cursor_error(
            "session id must contain only ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(value.to_string())
}

fn session_path(root: &Path, session_id: &str) -> Result<PathBuf, ErrorEnvelope> {
    let path = root.join(format!("{session_id}.jsonl"));
    if !path.is_file() {
        return Err(ErrorEnvelope::new(
            ErrorCode::SessionNotFound,
            format!("unknown session: {session_id}"),
        ));
    }
    let canonical_root = root.canonicalize().map_err(internal_io)?;
    let canonical_path = path.canonicalize().map_err(internal_io)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(ErrorEnvelope::new(
            ErrorCode::SandboxDenied,
            "session path resolves outside the configured root",
        ));
    }
    Ok(canonical_path)
}

fn timeline_response(root: &Path, query: SessionTimelineQuery, generation: u64) -> HttpResponse {
    let result = (|| {
        let path = session_path(root, &query.session_id)?;
        let events = replay_session(&path).map_err(internal_io)?;
        let all = timeline_page(&events, None, usize::MAX).map_err(internal_io)?;
        let cursor = usize::try_from(query.cursor.unwrap_or(0))
            .map_err(|_| cursor_error("timeline cursor exceeds platform limits"))?;
        if cursor > all.turns.len() || (cursor == all.turns.len() && !all.turns.is_empty()) {
            return Err(cursor_error(format!(
                "timeline cursor {cursor} is outside {} turns",
                all.turns.len()
            )));
        }
        let limit = query.limit as usize;
        let end = cursor.saturating_add(limit).min(all.turns.len());
        let turns = all.turns[cursor..end]
            .iter()
            .map(|turn| SessionTurnSummary {
                turn_id: turn.turn_id,
                start_seq: turn.start_seq,
                end_seq: turn.end_seq,
                status: match turn.status {
                    TurnStatus::Completed => SessionTurnStatus::Completed,
                    TurnStatus::Aborted => SessionTurnStatus::Aborted,
                    TurnStatus::Active => SessionTurnStatus::Active,
                },
            })
            .collect();
        Ok(SessionTimelinePage {
            session_id: query.session_id,
            connection_generation: generation,
            turns,
            next_cursor: (end < all.turns.len()).then_some(end as u64),
        })
    })();
    match result {
        Ok(page) => timeline_json_response(&page),
        Err(error) => rpc_error_response(error),
    }
}

fn event_response(root: &Path, query: SessionEventQuery, generation: u64) -> HttpResponse {
    event_response_with_hook(root, query, generation, || {})
}

fn event_response_with_hook<F>(
    root: &Path,
    query: SessionEventQuery,
    generation: u64,
    after_page: F,
) -> HttpResponse
where
    F: FnOnce(),
{
    let result = (|| {
        let path = session_path(root, &query.session_id)?;
        // Open the follow source before reading history. Appends that race with
        // the page read remain visible on this same file description.
        let file = File::open(&path).map_err(internal_io)?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        read_follow_events(&mut reader, &mut events)?;
        after_page();
        read_follow_events(&mut reader, &mut events)?;
        for (expected, event) in events.iter().enumerate() {
            if event.seq != expected as u64 {
                return Err(ErrorEnvelope::new(
                    ErrorCode::Internal,
                    format!("session event sequence gap at {expected}"),
                ));
            }
        }
        let start = match query.last_seq {
            None => 0,
            Some(last_seq) => {
                let next = last_seq
                    .checked_add(1)
                    .ok_or_else(|| cursor_error("last_seq overflow"))?;
                let next = usize::try_from(next)
                    .map_err(|_| cursor_error("last_seq exceeds platform limits"))?;
                if next > events.len() {
                    return Err(cursor_error(format!(
                        "last_seq {last_seq} is outside the session"
                    )));
                }
                next
            }
        };
        let mut body = String::new();
        for record in &events[start..] {
            let frame = SessionEventFrame {
                session_id: query.session_id.clone(),
                connection_generation: generation,
                record: record.clone(),
            };
            let data = serde_json::to_string(&frame).map_err(|error| {
                ErrorEnvelope::new(ErrorCode::Internal, format!("SSE encoding failed: {error}"))
            })?;
            body.push_str(&format!(
                "id: {}\nevent: session_event\ndata: {data}\n\n",
                record.seq
            ));
        }
        Ok(body)
    })();
    match result {
        Ok(body) => HttpResponse {
            status: 200,
            content_type: "text/event-stream",
            body,
        },
        Err(error) => rpc_error_response(error),
    }
}

fn read_follow_events(
    reader: &mut BufReader<File>,
    events: &mut Vec<protocol::SequencedEvent>,
) -> Result<(), ErrorEnvelope> {
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).map_err(internal_io)?;
        if bytes == 0 {
            return Ok(());
        }
        let event = serde_json::from_str(line.trim_end()).map_err(|error| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("session follow decode failed: {error}"),
            )
        })?;
        events.push(event);
    }
}

fn capabilities_response(generation: u64) -> HttpResponse {
    json_response(&SessionRpcCapabilities {
        connection_generation: generation,
        read_only: true,
        timeline: true,
        event_stream: true,
        last_event_id: true,
        follow_before_page: true,
        retry_business_errors: false,
    })
}

fn rpc_error_response(error: ErrorEnvelope) -> HttpResponse {
    let status = match error.code {
        ErrorCode::SessionNotFound => 404,
        ErrorCode::CursorInvalid | ErrorCode::ToolArgsInvalid => 400,
        ErrorCode::TeamRevisionConflict
        | ErrorCode::TeamDependencyCycle
        | ErrorCode::FileRevisionConflict => 409,
        ErrorCode::SandboxDenied | ErrorCode::ApprovalRejected => 403,
        _ => 500,
    };
    error_response(status, error)
}

fn timeline_json_response(value: &SessionTimelinePage) -> HttpResponse {
    json_response(value)
}

fn json_response<T: serde::Serialize>(value: &T) -> HttpResponse {
    match serde_json::to_string(value) {
        Ok(body) => HttpResponse {
            status: 200,
            content_type: "application/json",
            body,
        },
        Err(error) => error_response(
            500,
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("JSON encoding failed: {error}"),
            ),
        ),
    }
}

fn error_response(status: u16, error: ErrorEnvelope) -> HttpResponse {
    let body = serde_json::to_string(&RpcErrorResponse {
        error,
        retryable: false,
    })
    .unwrap_or_else(|_| r#"{"error":{"code":"internal","message":"encoding failed"}}"#.into());
    HttpResponse {
        status,
        content_type: "application/json",
        body,
    }
}

fn cursor_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::CursorInvalid, message)
}

fn internal_io(error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("session replay failed: {error}"),
    )
}

#[cfg(test)]
mod tests;
