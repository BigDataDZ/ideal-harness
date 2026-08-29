//! TASK-504 server acceptance tests.

use super::*;
use protocol::{Event, RpcErrorResponse, SessionEventFrame, SessionTimelinePage};
use session::JsonlSession;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ih-rpc-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn session(root: &Path, id: &str) -> JsonlSession {
    JsonlSession::create(root.join(format!("{id}.jsonl"))).unwrap()
}

fn error(response: &HttpResponse) -> RpcErrorResponse {
    serde_json::from_str(&response.body).unwrap()
}

fn sse_frames(response: &HttpResponse) -> Vec<SessionEventFrame> {
    response
        .body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}

#[test]
fn non_loopback_bind_and_missing_root_are_rejected() {
    let root = root("bind");
    assert!(ReadOnlySessionServer::bind(&root, "0.0.0.0:0".parse().unwrap()).is_err());
    assert!(
        ReadOnlySessionServer::bind(&root.join("missing"), "127.0.0.1:0".parse().unwrap()).is_err()
    );
    let server = ReadOnlySessionServer::bind(&root, "127.0.0.1:0".parse().unwrap()).unwrap();
    assert!(server.local_addr().unwrap().ip().is_loopback());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn loopback_http_transport_serves_protocol_json() {
    let root = root("transport");
    let mut source = session(&root, "demo");
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    source.append(Event::TurnCompleted { turn_id: 1 }).unwrap();
    let server = ReadOnlySessionServer::bind(&root, "127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = server.listener.accept().unwrap();
        handle_connection(stream, &server.root, route).unwrap();
    });

    let mut client = TcpStream::connect(address).unwrap();
    client
        .write_all(b"GET /v1/sessions/demo/timeline?limit=1 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    worker.join().unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    let body = response.split_once("\r\n\r\n").unwrap().1;
    let page: SessionTimelinePage = serde_json::from_str(body).unwrap();
    assert_eq!(page.session_id, "demo");
    assert_eq!(page.turns[0].turn_id, 1);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn timeline_is_replayed_from_source_on_every_request() {
    let root = root("timeline");
    let mut source = session(&root, "demo");
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    source.append(Event::TurnCompleted { turn_id: 1 }).unwrap();
    let first = route(&root, "GET", "/v1/sessions/demo/timeline/?limit=1");
    assert_eq!(first.status, 200);
    let first: SessionTimelinePage = serde_json::from_str(&first.body).unwrap();
    assert_eq!(first.turns.len(), 1);

    source.append(Event::TurnStarted { turn_id: 2 }).unwrap();
    source
        .append(Event::TurnAborted {
            turn_id: 2,
            reason: "stop".into(),
        })
        .unwrap();
    let second = route(&root, "GET", "/v1/sessions/demo/timeline/?cursor=1&limit=1");
    let second: SessionTimelinePage = serde_json::from_str(&second.body).unwrap();
    assert_eq!(second.turns[0].turn_id, 2);
    assert_eq!(second.turns[0].status, SessionTurnStatus::Aborted);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn sse_reconnect_from_last_seq_has_no_duplicate_or_gap() {
    let root = root("reconnect");
    let mut source = session(&root, "demo");
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    source
        .append(Event::UserMessage { text: "one".into() })
        .unwrap();
    let first = route(&root, "GET", "/v1/sessions/demo/events/");
    assert_eq!(first.content_type, "text/event-stream");
    assert_eq!(
        sse_frames(&first)
            .iter()
            .map(|frame| frame.record.seq)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );

    source.append(Event::TurnCompleted { turn_id: 1 }).unwrap();
    source.append(Event::TurnStarted { turn_id: 2 }).unwrap();
    let resumed = route(&root, "GET", "/v1/sessions/demo/events/?last_seq=1");
    assert_eq!(
        sse_frames(&resumed)
            .iter()
            .map(|frame| frame.record.seq)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let caught_up = route(&root, "GET", "/v1/sessions/demo/events/?last_seq=3");
    assert!(sse_frames(&caught_up).is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn bad_cursor_unknown_session_and_traversal_fail_closed() {
    let root = root("fail-closed");
    let mut source = session(&root, "demo");
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    source.append(Event::TurnCompleted { turn_id: 1 }).unwrap();

    let bad = route(&root, "GET", "/v1/sessions/demo/events/?last_seq=nope");
    assert_eq!(bad.status, 400);
    assert_eq!(error(&bad).error.code, ErrorCode::CursorInvalid);
    let ahead = route(&root, "GET", "/v1/sessions/demo/events/?last_seq=99");
    assert_eq!(error(&ahead).error.code, ErrorCode::CursorInvalid);
    let timeline_ahead = route(&root, "GET", "/v1/sessions/demo/timeline/?cursor=1&limit=1");
    assert_eq!(error(&timeline_ahead).error.code, ErrorCode::CursorInvalid);
    let missing = route(&root, "GET", "/v1/sessions/missing/timeline/");
    assert_eq!(missing.status, 404);
    assert_eq!(error(&missing).error.code, ErrorCode::SessionNotFound);
    let traversal = route(&root, "GET", "/v1/sessions/../events/");
    assert_eq!(traversal.status, 400);
    assert_eq!(error(&traversal).error.code, ErrorCode::CursorInvalid);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn write_methods_are_rejected_without_mutating_session() {
    let root = root("read-only");
    let mut source = session(&root, "demo");
    source.append(Event::TurnStarted { turn_id: 1 }).unwrap();
    let before = std::fs::read(root.join("demo.jsonl")).unwrap();
    let response = route(&root, "POST", "/v1/sessions/demo/events/");
    assert_eq!(response.status, 405);
    assert_eq!(error(&response).error.code, ErrorCode::SandboxDenied);
    assert_eq!(std::fs::read(root.join("demo.jsonl")).unwrap(), before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn cli_arguments_require_root_and_loopback_ip_address() {
    assert!(parse_args(&[]).is_err());
    assert!(parse_args(&["--root".into()]).is_err());
    assert!(parse_args(&[
        "--root".into(),
        "sessions".into(),
        "--bind".into(),
        "localhost:1".into()
    ])
    .is_err());
    let parsed = parse_args(&[
        "--root".into(),
        "sessions".into(),
        "--bind".into(),
        "[::1]:0".into(),
    ])
    .unwrap();
    assert!(parsed.bind.ip().is_loopback());
}
