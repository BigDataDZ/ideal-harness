//! TASK-504 minimal HTTP/1 transport; routing and DTO projection stay in the parent module.

use protocol::{ErrorCode, ErrorEnvelope, RpcErrorResponse};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;

pub(super) struct HttpResponse {
    pub(super) status: u16,
    pub(super) content_type: &'static str,
    pub(super) body: String,
}

pub(super) fn handle_connection(
    mut stream: TcpStream,
    root: &Path,
    route: fn(&Path, &str, &str) -> HttpResponse,
) -> std::io::Result<()> {
    let request = read_request(&stream)?;
    let response = match request {
        Ok(request) => route(root, &request.method, &request.target),
        Err(error) => bad_request(error),
    };
    write_response(&mut stream, &response)
}

struct HttpRequest {
    method: String,
    target: String,
}

fn read_request(stream: &TcpStream) -> std::io::Result<Result<HttpRequest, ErrorEnvelope>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    if first_line.len() > 8 * 1024 {
        return Ok(Err(invalid("HTTP request line is too large")));
    }
    let mut parts = first_line.split_whitespace();
    let request = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version), None) if version.starts_with("HTTP/1.") => {
            Ok(HttpRequest {
                method: method.to_string(),
                target: target.to_string(),
            })
        }
        _ => Err(invalid("malformed HTTP request line")),
    };
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if line.len() > 8 * 1024 {
            return Ok(Err(invalid("HTTP header line is too large")));
        }
    }
    Ok(request)
}

fn bad_request(error: ErrorEnvelope) -> HttpResponse {
    let body = serde_json::to_string(&RpcErrorResponse { error })
        .unwrap_or_else(|_| r#"{"error":{"code":"internal","message":"encoding failed"}}"#.into());
    HttpResponse {
        status: 400,
        content_type: "application/json",
        body,
    }
}

fn write_response(stream: &mut TcpStream, response: &HttpResponse) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
        response.body
    )?;
    stream.flush()
}

fn invalid(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::CursorInvalid, message)
}
