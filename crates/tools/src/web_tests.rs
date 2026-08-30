use super::*;
use protocol::{ErrorCode, ErrorEnvelope};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

struct MockFetcher {
    responses: Mutex<VecDeque<Result<FetchResponse, ErrorEnvelope>>>,
    requests: Mutex<Vec<String>>,
}

impl MockFetcher {
    fn ok(status: u16, location: Option<&str>, body: &str) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from([Ok(FetchResponse {
                status,
                location: location.map(str::to_owned),
                body: body.as_bytes().to_vec(),
                truncated: false,
            })])),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn chained(responses: Vec<FetchResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(
                responses.into_iter().map(Ok).collect::<Vec<_>>(),
            )),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Fetcher for MockFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, ErrorEnvelope> {
        self.requests.lock().unwrap().push(request.url.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock responses exhausted")
    }
}

fn allowlist() -> BTreeSet<String> {
    BTreeSet::from(["docs.example".to_string(), "alias.example".to_string()])
}

fn spill_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ih-web-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    root
}

fn success(value: protocol::ToolOutcome) -> serde_json::Value {
    match value {
        protocol::ToolOutcome::Success { value } => value,
        protocol::ToolOutcome::Failure { error } => panic!("expected success, got {error:?}"),
    }
}

fn failure(value: protocol::ToolOutcome) -> ErrorEnvelope {
    match value {
        protocol::ToolOutcome::Failure { error } => error,
        protocol::ToolOutcome::Success { value } => panic!("expected failure, got {value}"),
    }
}

#[test]
fn allowlisted_host_fetches_and_reports_status() {
    let fetcher = MockFetcher::ok(200, None, "# hello\nworld");
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("allowlisted"),
        ".harness/spill",
        64 * 1024,
    );
    let value = success(tool.fetch(&serde_json::json!({ "url": "https://docs.example/guide" })));
    assert_eq!(value["status"], 200);
    assert_eq!(value["content"], "# hello\nworld");
    assert_eq!(value["truncated"], false);
}

#[test]
fn private_loopback_and_non_allowlisted_hosts_are_denied() {
    let tool = WebFetchTool::new(
        Arc::new(MockFetcher::ok(200, None, "x")),
        allowlist(),
        spill_root("denied"),
        ".harness/spill",
        64 * 1024,
    );
    for (label, url) in [
        ("loopback", "http://127.0.0.1/x"),
        ("localhost", "http://localhost:3080/x"),
        ("ipv6-loopback", "http://[::1]/x"),
        ("private-10", "http://10.1.2.3/x"),
        ("private-192168", "http://192.168.1.4/x"),
        ("private-17216", "http://172.16.0.9/x"),
        ("link-local", "http://169.254.10.10/x"),
        ("mapped-v4", "http://[::ffff:127.0.0.1]/x"),
        ("ula-v6", "http://[fd00::1]/x"),
    ] {
        let error = failure(tool.fetch(&serde_json::json!({ "url": url })));
        assert_eq!(error.code, ErrorCode::SandboxDenied, "{label}");
        assert!(error.message.contains("not fetchable"), "{label}");
    }
    let error = failure(tool.fetch(&serde_json::json!({ "url": "https://evil.example/x" })));
    assert_eq!(error.code, ErrorCode::SandboxDenied);
    assert!(error.message.contains("not allowlisted"));
    // 非 http(s) scheme
    assert_eq!(
        failure(tool.fetch(&serde_json::json!({ "url": "ftp://docs.example/x" }))).code,
        ErrorCode::SandboxDenied
    );
}

#[test]
fn redirects_are_refollowed_only_within_policy() {
    // 合法重定向：目标在白名单内 → 跟随
    let fetcher = MockFetcher::chained(vec![
        FetchResponse {
            status: 301,
            location: Some("https://alias.example/final".into()),
            body: Vec::new(),
            truncated: false,
        },
        FetchResponse {
            status: 200,
            location: None,
            body: b"final".to_vec(),
            truncated: false,
        },
    ]);
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("redirect-ok"),
        ".harness/spill",
        64 * 1024,
    );
    let value = success(tool.fetch(&serde_json::json!({ "url": "https://docs.example/a" })));
    assert_eq!(value["url"], "https://alias.example/final");
    assert_eq!(value["content"], "final");

    // 重定向逃逸到私网 → 拒绝
    let fetcher = MockFetcher::chained(vec![FetchResponse {
        status: 302,
        location: Some("http://127.0.0.1/admin".into()),
        body: Vec::new(),
        truncated: false,
    }]);
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("redirect-private"),
        ".harness/spill",
        64 * 1024,
    );
    assert_eq!(
        failure(tool.fetch(&serde_json::json!({ "url": "https://docs.example/a" }))).code,
        ErrorCode::SandboxDenied
    );

    // 重定向逃逸到白名单外 → 拒绝
    let fetcher = MockFetcher::chained(vec![FetchResponse {
        status: 302,
        location: Some("https://evil.example/x".into()),
        body: Vec::new(),
        truncated: false,
    }]);
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("redirect-evil"),
        ".harness/spill",
        64 * 1024,
    );
    assert_eq!(
        failure(tool.fetch(&serde_json::json!({ "url": "https://docs.example/a" }))).code,
        ErrorCode::SandboxDenied
    );

    // 超过跳数上限 → 拒绝
    let hop = FetchResponse {
        status: 302,
        location: Some("https://docs.example/next".into()),
        body: Vec::new(),
        truncated: false,
    };
    let fetcher = MockFetcher::chained(vec![hop.clone(), hop.clone(), hop.clone(), hop]);
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("redirect-loop"),
        ".harness/spill",
        64 * 1024,
    );
    assert_eq!(
        failure(tool.fetch(&serde_json::json!({ "url": "https://docs.example/a" }))).code,
        ErrorCode::ToolArgsInvalid
    );
}

#[test]
fn oversized_content_spills_with_readable_locator_and_binary_is_rejected() {
    let body = format!("{}\nTAIL-MARKER\n", "y".repeat(40_000));
    let fetcher = MockFetcher::ok(200, None, &body);
    let root = spill_root("spill");
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        root.clone(),
        ".harness/spill",
        64 * 1024,
    );
    let value = success(tool.fetch(&serde_json::json!({ "url": "https://docs.example/big" })));
    assert_eq!(value["truncated"], true);
    let locator = value["locator"].as_str().unwrap();
    assert!(locator.starts_with(".harness/spill/"));
    let full = std::fs::read_to_string(root.join(locator.strip_prefix(".harness/spill/").unwrap()))
        .unwrap();
    assert!(full.ends_with("TAIL-MARKER\n"));
    assert_eq!(value["content"].as_str().unwrap().chars().count(), 4_000);

    let fetcher = MockFetcher::ok(200, None, "binary\0data");
    let tool = WebFetchTool::new(
        Arc::new(fetcher),
        allowlist(),
        spill_root("binary"),
        ".harness/spill",
        64 * 1024,
    );
    assert_eq!(
        failure(tool.fetch(&serde_json::json!({ "url": "https://docs.example/bin" }))).code,
        ErrorCode::ToolArgsInvalid
    );
}

#[test]
fn fetcher_transport_errors_pass_through_with_stable_code() {
    struct Broken;
    impl Fetcher for Broken {
        fn fetch(&self, _: &FetchRequest) -> Result<FetchResponse, ErrorEnvelope> {
            Err(ErrorEnvelope::new(ErrorCode::Internal, "proxy unreachable"))
        }
    }
    let tool = WebFetchTool::new(
        Arc::new(Broken),
        allowlist(),
        spill_root("broken"),
        ".harness/spill",
        64 * 1024,
    );
    let error = failure(tool.fetch(&serde_json::json!({ "url": "https://docs.example/x" })));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("proxy unreachable"));
}

#[test]
fn url_host_parsing_edges() {
    assert!(is_private_host("LOCALHOST."));
    assert!(is_private_host("sub.localhost"));
    assert!(!is_private_host("docs.example"));
    assert!(
        !is_private_host("example.127.0.0.1.nip.io"),
        "DNS 级钉扎属于物理通道职责"
    );
    assert_eq!(
        parse_url("https://Docs.Example:8443/a?b#c").unwrap().1,
        "docs.example"
    );
    assert_eq!(parse_url("http://[FD00::1]:8080/x").unwrap().1, "[fd00::1]");
    assert!(parse_url("not-a-url").is_err());
    assert!(parse_url("https:///no-host").is_err());
}
