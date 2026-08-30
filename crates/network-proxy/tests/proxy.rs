//! TASK-203 验收：默认断网、白名单转发、拒绝审计事件。

use network_proxy::{ProxyPolicy, ProxyServer};
use protocol::Event;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

#[test]
fn deny_all_rejects_and_records_stable_audit_event() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let server = ProxyServer::bind(localhost(0), ProxyPolicy::deny_all(), move |event| {
        captured.lock().unwrap().push(event);
        Ok(())
    })
    .unwrap();
    let address = server.local_addr().unwrap();
    let worker = thread::spawn(move || server.serve_once());

    let mut client = TcpStream::connect(address).unwrap();
    client
        .write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\nHost: blocked.example\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    worker.join().unwrap().unwrap();

    assert!(response.starts_with("HTTP/1.1 403"));
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [Event::NetworkAccessDenied {
            host: "blocked.example".into(),
            port: 443,
            reason: "host_not_allowlisted".into(),
        }]
    );
}

#[test]
fn explicitly_allowed_provider_host_tunnels_bytes() {
    let upstream = TcpListener::bind(localhost(0)).unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let upstream_worker = thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut payload = [0u8; 4];
        stream.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"ping");
        stream.write_all(b"pong").unwrap();
    });

    let mut policy = ProxyPolicy::for_provider("127.0.0.1").unwrap();
    policy.allow_forbidden_targets();
    let server = ProxyServer::bind(localhost(0), policy, |_| Ok(())).unwrap();
    let proxy_address = server.local_addr().unwrap();
    let proxy_worker = thread::spawn(move || server.serve_once());

    let mut client = TcpStream::connect(proxy_address).unwrap();
    write!(
        client,
        "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        client.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }
    assert!(response.starts_with(b"HTTP/1.1 200"));
    client.write_all(b"ping").unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"pong");
    drop(client);

    upstream_worker.join().unwrap();
    proxy_worker.join().unwrap().unwrap();
}

#[test]
fn audit_sink_failure_does_not_turn_denial_into_access() {
    let server = ProxyServer::bind(localhost(0), ProxyPolicy::deny_all(), |_| {
        Err(std::io::Error::other("audit unavailable"))
    })
    .unwrap();
    let address = server.local_addr().unwrap();
    let worker = thread::spawn(move || server.serve_once());

    let mut client = TcpStream::connect(address).unwrap();
    client
        .write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403"));
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn continuous_server_stops_after_handling_multiple_connections() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let server = ProxyServer::bind(localhost(0), ProxyPolicy::deny_all(), move |event| {
        captured.lock().unwrap().push(event);
        Ok(())
    })
    .unwrap();
    let address = server.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || server.serve_until(&worker_stop));

    for host in ["one.example", "two.example"] {
        let mut client = TcpStream::connect(address).unwrap();
        write!(client, "CONNECT {host}:443 HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
    }
    stop.store(true, Ordering::Release);
    worker.join().unwrap().unwrap();
    assert_eq!(events.lock().unwrap().len(), 2);
}
