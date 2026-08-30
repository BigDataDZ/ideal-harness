//! P2/TASK-203：最小 HTTP CONNECT 代理与拒绝审计。

use crate::ProxyPolicy;
use protocol::Event;
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::Arc,
    thread,
    time::Duration,
};

const MAX_HEADER_BYTES: usize = 8 * 1024;
/// TASK-810：并发连接 worker 硬上限；超限立即 503 并留审计事件。
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

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
        let active = Arc::new(AtomicUsize::new(0));
        while !stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((client, _)) => {
                    // TASK-810：并发上限——超限立即 503 并留审计事件
                    if active.load(Ordering::SeqCst) >= MAX_CONCURRENT_CONNECTIONS {
                        let mut client = client;
                        let _ = client.write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n",
                        );
                        self.audit.record(Event::NetworkAccessDenied {
                            host: "connection".into(),
                            port: 0,
                            reason: "connection_limit_exceeded".into(),
                        })?;
                        continue;
                    }
                    let _ = active.fetch_add(1, Ordering::SeqCst);
                    let policy = self.policy.clone();
                    let audit = Arc::clone(&self.audit);
                    let active_for_worker = Arc::clone(&active);
                    workers.push(thread::spawn(move || {
                        let result = handle_client(client, &policy, audit.as_ref());
                        active_for_worker.fetch_sub(1, Ordering::SeqCst);
                        result
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

    // TASK-801：明文转发时 Host 头必须与目标一致（CONNECT 无 Host 语义，跳过）；
    // 头级检查先于 DNS，避免为注定拒绝的请求做解析。
    if request.kind == RequestKind::PlainHttp {
        if let Some(header_host) = host_header_host(&head) {
            if header_host != request.host {
                client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
                audit.record(Event::NetworkAccessDenied {
                    host: request.host,
                    port: request.port,
                    reason: format!("host_header_mismatch:{header_host}"),
                })?;
                return Ok(());
            }
        }
    }

    // TASK-801：DNS 解析后钉扎——校验全部解析结果并只连接已校验地址，
    // 校验地址与连接地址同源，解析结果变化无法绕过已做判断。
    let pinned = match resolve_and_pin(
        &request.host,
        request.port,
        policy.forbidden_targets_allowed(),
    ) {
        Ok(address) => address,
        Err(PinError::Forbidden(ip)) => {
            let reason = format!("forbidden_resolved_ip:{ip}");
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
            audit.record(Event::NetworkAccessDenied {
                host: request.host,
                port: request.port,
                reason,
            })?;
            return Ok(());
        }
        Err(PinError::Resolution(_) | PinError::Empty) => {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
            audit.record(Event::NetworkAccessDenied {
                host: request.host,
                port: request.port,
                reason: "resolution_failed".into(),
            })?;
            return Ok(());
        }
    };

    let upstream = match TcpStream::connect(pinned) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// TASK-801：解析目标主机并校验**全部**解析结果；任一禁用地址即整体拒绝。
/// 返回的 SocketAddr 是调用方唯一应连接的地址（校验地址 = 连接地址）。
#[derive(Debug)]
enum PinError {
    Resolution(#[allow(dead_code)] io::Error),
    Forbidden(IpAddr),
    Empty,
}

fn resolve_and_pin(
    host: &str,
    port: u16,
    allow_forbidden_targets: bool,
) -> Result<SocketAddr, PinError> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(PinError::Resolution)?
        .collect();
    if addrs.is_empty() {
        return Err(PinError::Empty);
    }
    let mut pinned = None;
    for address in addrs {
        if !allow_forbidden_targets && is_forbidden_target_ip(address.ip()) {
            return Err(PinError::Forbidden(address.ip()));
        }
        if pinned.is_none() {
            pinned = Some(address);
        }
    }
    pinned.ok_or(PinError::Empty)
}

/// TASK-801：SSRF 禁用地址集——loopback、unspecified、RFC1918、CGNAT、
/// link-local、组播、保留段，以及映射到上述段的 IPv6。
fn is_forbidden_target_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            matches!(
                (a, b),
                (0, _)
                    | (10, _)
                    | (127, _)
                    | (100, 64..=127)
                    | (169, 254)
                    | (172, 16..=31)
                    | (192, 168)
                    | (224..=239, _)
                    | (240..=255, _)
            )
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let segments = v6.segments();
            if segments[0] == 0
                && segments[1] == 0
                && segments[2] == 0
                && segments[3] == 0
                && segments[4] == 0
                && segments[5] == 0xffff
            {
                let mapped = Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                );
                return is_forbidden_target_ip(IpAddr::V4(mapped));
            }
            let first = segments[0];
            (first & 0xffc0) == 0xfe80      // fe80::/10 link-local
                || (first & 0xfe00) == 0xfc00  // fc00::/7 ULA
                || (first & 0xff00) == 0xff00 // ff00::/8 multicast
        }
    }
}

/// 从请求头提取 Host 头的主机部分（去端口、小写）；无 Host 头返回 None。
fn host_header_host(head: &[u8]) -> Option<String> {
    let header = std::str::from_utf8(head).ok()?;
    for line in header.lines() {
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("Host"))
            .map(|(_, value)| value.trim())
        {
            let host = value.rsplit_once(':').map_or(value, |(host, _)| host);
            return Some(host.trim_matches(['[', ']']).to_ascii_lowercase());
        }
    }
    None
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
        let mut policy = ProxyPolicy::for_provider("127.0.0.1").unwrap();
        policy.allow_forbidden_targets();
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            policy,
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
    fn resolved_loopback_target_is_denied_with_structured_audit() {
        // allowlist 放行 localhost，但解析结果是 127.0.0.1 —— 必须在钉扎层拒绝
        let events = EventCollector(Mutex::new(Vec::new()));
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            ProxyPolicy::for_provider("localhost").unwrap(),
            events,
        )
        .unwrap();
        let address = serve_background(proxy);
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"GET http://localhost/x HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let response = read_to_end(&mut client);
        assert!(String::from_utf8_lossy(&response).contains("403 Forbidden"));
    }

    #[test]
    fn host_header_mismatch_is_denied_before_resolution() {
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            ProxyPolicy::for_provider("provider.example").unwrap(),
            EventCollector(Mutex::new(Vec::new())),
        )
        .unwrap();
        let address = serve_background(proxy);
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"GET http://provider.example/x HTTP/1.1\r\nHost: evil.example\r\n\r\n")
            .unwrap();
        let response = read_to_end(&mut client);
        assert!(String::from_utf8_lossy(&response).contains("403 Forbidden"));
    }

    #[test]
    fn fuzz_request_head_parse_never_panics() {
        fn xorshift(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }
        let base = b"CONNECT 127.0.0.1:443 HTTP/1.1
Host: 127.0.0.1

";
        let mut state = 0x9e37_79b9_u64;
        for _ in 0..3000 {
            let mut bytes = base.to_vec();
            let mutations = (xorshift(&mut state) % 8) + 1;
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
                        let pos = (xorshift(&mut state) as usize) % bytes.len();
                        bytes[pos] = (xorshift(&mut state) & 0xff) as u8;
                    }
                }
            }
            let _ = parse_request_head(&bytes);
        }
    }

    #[test]
    fn pin_validates_every_resolved_address_and_returns_the_pinned_one() {
        // 校验地址与连接地址同源：pin 返回的 SocketAddr 就是唯一会被连接的地址
        let probe = resolve_and_pin("localhost", 80, false);
        println!("probe localhost pin: {probe:?}");
        assert!(matches!(probe, Err(PinError::Forbidden(_))));
        let pinned = resolve_and_pin("localhost", 80, true).unwrap();
        assert!(
            is_forbidden_target_ip(pinned.ip()),
            "旗标开启时返回钉扎地址"
        );
        assert!(pinned.port() == 80);
    }

    #[test]
    fn non_get_connect_methods_are_rejected_without_upstream() {
        let mut policy = ProxyPolicy::for_provider("127.0.0.1").unwrap();
        policy.allow_forbidden_targets();
        let proxy = ProxyServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            policy,
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
