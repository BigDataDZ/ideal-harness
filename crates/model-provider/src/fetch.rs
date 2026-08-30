//! TASK-703：经代理的原始 HTTP GET——禁用自动重定向，字节上限硬截断。
//! SSRF 分类与主机白名单策略在 `tools::WebFetchTool`；本模块只负责物理传输。

use crate::map_transport_error;
use protocol::{ErrorCode, ErrorEnvelope};
use std::time::Duration;

/// 一次物理取回的结果；`truncated` 表示 body 因 max_bytes 被硬截断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFetchOutcome {
    pub status: u16,
    pub location: Option<String>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// 单次 GET。`proxy_url` 为 Some 时强制所有流量经该代理（仅接受回环 http 代理，
/// 与模型路径同一安全语义）；实现禁用自动重定向——重定向复检属策略层职责。
pub fn http_fetch_via_proxy(
    proxy_url: Option<&str>,
    url: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<HttpFetchOutcome, ErrorEnvelope> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy_url) = proxy_url {
        let proxy = reqwest::Url::parse(proxy_url)
            .map_err(|e| ErrorEnvelope::new(ErrorCode::Internal, format!("代理 URL 非法: {e}")))?;
        if proxy.scheme() != "http" || !crate::url_is_loopback(&proxy) {
            return Err(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "fetch 代理必须是本机回环地址上的 http:// 端点",
            ));
        }
        let configured = reqwest::Proxy::all(proxy.as_str())
            .map_err(|e| ErrorEnvelope::new(ErrorCode::Internal, format!("代理配置失败: {e}")))?;
        builder = builder.proxy(configured);
    }
    let http = builder.build().map_err(|e| {
        ErrorEnvelope::new(ErrorCode::Internal, format!("HTTP 客户端初始化失败: {e}"))
    })?;
    let response = http
        .get(url)
        .header(reqwest::header::ACCEPT, "text/*, */*;q=0.5")
        .send()
        .map_err(map_transport_error)?;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().map_err(map_transport_error)?;
    let mut body = bytes.to_vec();
    let mut truncated = false;
    if body.len() > max_bytes {
        body.truncate(max_bytes);
        truncated = true;
    }
    Ok(HttpFetchOutcome {
        status,
        location,
        body,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 本地回环监听器返回固定 HTTP 响应，并回传收到的请求行供断言。
    fn serve(response: &'static str) -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_for_thread = seen.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or(0);
                *seen_for_thread.lock().unwrap() =
                    String::from_utf8_lossy(&buffer[..read]).into_owned();
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://127.0.0.1:{port}/guide"), seen)
    }

    #[test]
    fn fetches_body_and_reports_status_without_following_redirects() {
        let (url, seen) = serve(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://elsewhere/x\r\nContent-Length: 0\r\n\r\n",
        );
        let outcome = http_fetch_via_proxy(None, &url, 64 * 1024, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome.status, 301);
        assert_eq!(outcome.location.as_deref(), Some("https://elsewhere/x"));
        assert!(outcome.body.is_empty());
        assert!(!outcome.truncated);
        assert!(seen.lock().unwrap().starts_with("GET /guide"));
    }

    #[test]
    fn truncates_body_at_hard_byte_limit() {
        let (url, _) = serve("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabcdefghij");
        let outcome = http_fetch_via_proxy(None, &url, 4, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome.body, b"abcd".to_vec());
        assert!(outcome.truncated);
    }

    #[test]
    fn non_loopback_proxy_is_rejected_before_any_request() {
        let error = http_fetch_via_proxy(
            Some("http://proxy.example:3128"),
            "http://127.0.0.1:9/x",
            1024,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::SandboxDenied);
    }
}
