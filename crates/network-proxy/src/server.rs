//! P2/TASK-203：最小 HTTP CONNECT 代理与拒绝审计。

use crate::ProxyPolicy;
use protocol::Event;
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
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
        let (mut client, _) = self.listener.accept()?;
        client.set_read_timeout(Some(Duration::from_secs(10)))?;
        client.set_write_timeout(Some(Duration::from_secs(10)))?;
        let request = read_connect_request(&mut client)?;

        if !self.policy.allows(&request.host) {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
            self.audit.record(Event::NetworkAccessDenied {
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
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        tunnel(client, upstream)
    }
}

struct ConnectRequest {
    host: String,
    port: u16,
}

fn read_connect_request(stream: &mut TcpStream) -> io::Result<ConnectRequest> {
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
            "CONNECT header is incomplete or exceeds 8 KiB",
        ));
    }
    let header = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header is not UTF-8"))?;
    let first_line = header.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let authority = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "CONNECT" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only HTTP/1.x CONNECT requests are supported",
        ));
    }
    parse_authority(authority)
}

fn parse_authority(authority: &str) -> io::Result<ConnectRequest> {
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "CONNECT target requires host:port",
        )
    })?;
    if host.is_empty()
        || host.contains([':', '/', '\\', '@', '*'])
        || host.chars().any(char::is_whitespace)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CONNECT target contains an invalid host",
        ));
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid CONNECT port"))?;
    Ok(ConnectRequest {
        host: host.to_ascii_lowercase(),
        port,
    })
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
