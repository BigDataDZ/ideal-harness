//! P5/TASK-504：loopback-only 只读 HTTP RPC 与按 seq SSE 补洞。

use protocol::{
    ErrorCode, ErrorEnvelope, RpcErrorResponse, SessionEventFrame, SessionEventQuery,
    SessionTimelinePage, SessionTimelineQuery, SessionTurnStatus, SessionTurnSummary,
};
use session::{replay_session, timeline_page, TurnStatus};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};

mod http;
use http::{handle_connection, HttpResponse};

const DEFAULT_BIND: &str = "127.0.0.1:8765";
const DEFAULT_TIMELINE_LIMIT: u32 = 20;
const MAX_TIMELINE_LIMIT: u32 = 1_000;

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
        Ok(Self {
            listener: TcpListener::bind(address)?,
            root: root.to_path_buf(),
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn run(self) -> anyhow::Result<()> {
        for stream in self.listener.incoming() {
            handle_connection(stream?, &self.root, route)?;
        }
        Ok(())
    }
}

fn route(root: &Path, method: &str, target: &str) -> HttpResponse {
    if method != "GET" {
        return error_response(
            405,
            ErrorEnvelope::new(ErrorCode::SandboxDenied, "read-only RPC accepts GET only"),
        );
    }
    match parse_target(target) {
        Ok(Route::Timeline(query)) => timeline_response(root, query),
        Ok(Route::Events(query)) => event_response(root, query),
        Err(error) => error_response(400, error),
    }
}

enum Route {
    Timeline(SessionTimelineQuery),
    Events(SessionEventQuery),
}

fn parse_target(target: &str) -> Result<Route, ErrorEnvelope> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts.len() != 4 || parts[0] != "v1" || parts[1] != "sessions" {
        return Err(cursor_error("unknown read-only RPC route"));
    }
    let session_id = validate_session_id(parts[2])?;
    match parts[3] {
        "timeline" => {
            let values = parse_query(query, &["cursor", "limit"])?;
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
                cursor,
                limit,
            }))
        }
        "events" => {
            let values = parse_query(query, &["last_seq"])?;
            Ok(Route::Events(SessionEventQuery {
                session_id,
                last_seq: parse_optional_u64(values.get("last_seq").copied(), "last_seq")?,
            }))
        }
        _ => Err(cursor_error("unknown read-only RPC route")),
    }
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

fn timeline_response(root: &Path, query: SessionTimelineQuery) -> HttpResponse {
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
            turns,
            next_cursor: (end < all.turns.len()).then_some(end as u64),
        })
    })();
    match result {
        Ok(page) => timeline_json_response(&page),
        Err(error) => rpc_error_response(error),
    }
}

fn event_response(root: &Path, query: SessionEventQuery) -> HttpResponse {
    let result = (|| {
        let path = session_path(root, &query.session_id)?;
        let events = replay_session(&path).map_err(internal_io)?;
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

fn rpc_error_response(error: ErrorEnvelope) -> HttpResponse {
    let status = match error.code {
        ErrorCode::SessionNotFound => 404,
        ErrorCode::CursorInvalid | ErrorCode::ToolArgsInvalid => 400,
        ErrorCode::SandboxDenied | ErrorCode::ApprovalRejected => 403,
        _ => 500,
    };
    error_response(status, error)
}

fn timeline_json_response(value: &SessionTimelinePage) -> HttpResponse {
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
    let body = serde_json::to_string(&RpcErrorResponse { error })
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
