//! P2/TASK-203：最小 HTTP CONNECT 代理与拒绝审计。

use crate::ProxyPolicy;
use protocol::Event;
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::Duration,
};

const MAX_HEADER_BYTES: usize = 8 * 1024;

pub trait AuditSink: Send + Sync {
    fn record(&self, event: Event) -> io::Result<()>;
}

impl<F> AuditSink for F
where
    F: Fn(Event) -> io::Result<()> + Send + Sync,
{
    fn record(&self, event: Event) -> io::Result<()> {
        self(event)
    }
}

pub struct ProxyServer<S> {
    listener: TcpListener,
    policy: ProxyPolicy,
    audit: Arc<S>,
}

impl<S: AuditSink + 'static> ProxyServer<S> {
    pub fn bind(address: SocketAddr, policy: ProxyPolicy, audit: S) -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(address)?,
            policy,
            audit: Arc::new(audit),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 处理一个连接，供独立代理进程的 accept loop 调用。
    pub fn serve_once(&self) -> io::Result<()> {
        let (client, _) = self.listener.accept()?;
        handle_client(client, &self.policy, self.audit.as_ref())
    }

    /// 持续处理连接直到收到停止信号。每条隧道独立线程处理，accept loop 保持可停止。
    pub fn serve_until(&self, stop: &AtomicBool) -> io::Result<()> {
        self.listener.set_nonblocking(true)?;
        let mut workers = Vec::new();
        while !stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((client, _)) => {
                    let policy = self.policy.clone();
                    let audit = Arc::clone(&self.audit);
                    workers.push(thread::spawn(move || {
                        handle_client(client, &policy, audit.as_ref())
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| io::Error::other("proxy connection thread panicked"))??;
        }
        Ok(())
    }
}

fn handle_client<S: AuditSink>(
    mut client: TcpStream,
    policy: &ProxyPolicy,
    audit: &S,
) -> io::Result<()> {
    // serve_until 的 listener 是非阻塞的；Windows 上 accept 出来的 socket 会继承
    // 非阻塞模式，必须显式恢复阻塞，否则读会以 WSAEWOULDBLOCK 立即失败。
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    client.set_write_timeout(Some(Duration::from_secs(10)))?;
    let head = read_request_head(&mut client)?;
    let request = parse_request_head(&head)?;

    if !policy.allows(&request.host) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
        audit.record(Event::NetworkAccessDenied {
            host: request.host,
            port: request.port,
            reason: "host_not_allowlisted".into(),
        })?;
        return Ok(());
    }

    let upstream = match TcpStream::connect((request.host.as_str(), request.port)) {
        Ok(upstream) => upstream,
        Err(error) => {
            client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")?;
            return Err(error);
        }
    };
    match request.kind {
        RequestKind::Connect => {
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        }
        // TASK-703 补口：absolute-form 的明文 GET/HEAD 原样转发（绝对式 URI 由
        // 源站按 RFC 9112 处理）。请求头已被 read_request_head 消费，必须先
        // 补写给上游；响应边界由客户端按 Content-Length/chunked 解析。
        RequestKind::PlainHttp => {
            let mut upstream_writer = upstream.try_clone()?;
            upstream_writer.write_all(&head)?;
            upstream_writer.flush()?;
        }
    }
    tunnel(client, upstream)
}

struct ConnectRequest {
    kind: RequestKind,
    host: String,
    port: u16,
}

enum RequestKind {
    /// 隧道化 HTTPS：已建立上游后双向透传。
    Connect,
    /// 明文 HTTP：absolute-form GET/HEAD 原样转发到白名单内的源站。
    PlainHttp,
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    while bytes.len() < MAX_HEADER_BYTES {
        if stream.read(&mut one)? == 0 {
            break;
        }
        bytes.push(one[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    if !bytes.ends_with(b"\r\n\r\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request head is incomplete or exceeds 8 KiB",
        ));
    }
    Ok(bytes)
}

fn parse_request_head(head: &[u8]) -> io::Result<ConnectRequest> {
    let header = std::str::from_utf8(head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header is not UTF-8"))?;
    let first_line = header.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only HTTP/1.x requests are supported",
        ));
    }
    match method {
        "CONNECT" => {
            let (host, port) = parse_authority(target, "CONNECT target requires host:port")?;
            Ok(ConnectRequest {
                kind: RequestKind::Connect,
                host,
                port,
            })
        }
        // 明文转发只放行幂等且无请求体的方法；GET/HEAD 之外的按坏请求拒绝
        "GET" | "HEAD" => {
            let authority = target
                .split_once("://")
                .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "target must be absolute-form")
                })?;
            let (host, port) = match authority.rsplit_once(':') {
                Some((host, port)) => (
                    host,
                    port.parse::<u16>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid target port")
                    })?,
                ),
                None => (authority, 80),
            };
            if host.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "target has no host",
                ));
            }
            let (host, port) = parse_authority(&format!("{host}:{port}"), "invalid target")?;
            Ok(ConnectRequest {
                kind: RequestKind::PlainHttp,
                host,
                port,
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only CONNECT and plain-form GET/HEAD requests are supported",
        )),
    }
}

fn parse_authority(authority: &str, message: &str) -> io::Result<(String, u16)> {
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, message))?;
    if host.is_empty()
        || host.contains([':', '/', '\\', '@', '*'])
        || host.chars().any(char::is_whitespace)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "target contains an invalid host",
        ));
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid port"))?;
    Ok((host.to_ascii_lowercase(), port))
}

fn tunnel(client: TcpStream, upstream: TcpStream) -> io::Result<()> {
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let client_to_upstream = thread::spawn(move || {
        let result = io::copy(&mut client_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
        result
    });

    let mut upstream_reader = upstream;
    let mut client_writer = client;
    let upstream_to_client = io::copy(&mut upstream_reader, &mut client_writer);
    let _ = client_writer.shutdown(Shutdown::Write);
    let client_to_upstream = client_to_upstream
        .join()
        .map_err(|_| io::Error::other("proxy tunnel thread panicked"))?;
    upstream_to_client?;
    client_to_upstream?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct EventCollector(Mutex<Vec<Event>>);

    impl AuditSink for EventCollector {
        fn record(&self, event: Event) -> io::Result<()> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    /// 起一个返回固定响应的源站，返回其地址。
    fn origin(response: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        address
    }

    fn serve_background<S: AuditSink + 'static>(proxy: ProxyServer<S>) -> SocketAddr {
        let address = proxy.local_addr().unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            let _ = proxy.serve_until(&stop_for_thread);
        });
        address
    }

    fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 512];
        while let Ok(read) = stream.read(&mut chunk) {
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        buffer
    }

    #[test]
    fn plain_http_get_to_allowlisted_host_is_forwarded() {
        let upstream = origin("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            ProxyPolicy::for_provider("127.0.0.1").unwrap(),
            EventCollector(Mutex::new(Vec::new())),
        )
        .unwrap();
        let address = serve_background(proxy);

        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(format!("GET http://{upstream}/guide HTTP/1.1\r\n\r\n").as_bytes())
            .unwrap();
        let response = read_to_end(&mut client);
        assert!(
            response.windows(5).any(|window| window == b"hello"),
            "{:?}",
            String::from_utf8_lossy(&response)
        );
    }

    #[test]
    fn plain_http_get_to_unlisted_host_is_denied_with_audit() {
        let events = EventCollector(Mutex::new(Vec::new()));
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            ProxyPolicy::for_provider("provider.example").unwrap(),
            events,
        )
        .unwrap();
        let address = serve_background(proxy);

        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"GET http://unlisted.example/x HTTP/1.1\r\nHost: unlisted.example\r\n\r\n")
            .unwrap();
        let response = read_to_end(&mut client);
        assert!(String::from_utf8_lossy(&response).contains("403 Forbidden"));
    }

    #[test]
    fn non_get_connect_methods_are_rejected_without_upstream() {
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            ProxyPolicy::for_provider("127.0.0.1").unwrap(),
            EventCollector(Mutex::new(Vec::new())),
        )
        .unwrap();
        let address = serve_background(proxy);

        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"POST http://127.0.0.1/x HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        // 坏请求：连接被直接关闭，无任何上游转发
        let response = read_to_end(&mut client);
        assert!(response.is_empty());
    }
}
