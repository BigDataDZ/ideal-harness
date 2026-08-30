//! TASK-703：白名单代理 web_fetch 工具。
//! 策略三层：仅 http/https、私网/回环主机一律拒绝（SSRF）、
//! 主机必须显式列入 allowlist（默认拒绝）；重定向逐跳复检；
//! 超限内容全文落 spill，结果只带预览 + locator。
//! 物理出网通道由 `Fetcher` 实现提供（装配层接入 CONNECT 白名单代理）。

use protocol::{ErrorCode, ErrorEnvelope};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const MAX_REDIRECT_HOPS: usize = 3;
const MAX_CONTENT_CHARS: usize = 20_000;
const PREVIEW_CHARS: usize = 4_000;
const MAX_SPILL_BYTES: usize = 8 * 1024 * 1024;

/// 一次取回请求；物理实现必须尊重 max_bytes 硬上限并禁用自动重定向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub status: u16,
    /// 3xx 时的 Location 头；实现不得自行跟随重定向。
    pub location: Option<String>,
    pub body: Vec<u8>,
    /// true 表示实现侧因 max_bytes 截断了 body。
    pub truncated: bool,
}

/// 物理取回通道抽象；生产实现经 CONNECT 白名单代理出网。
pub trait Fetcher: Send + Sync {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, ErrorEnvelope>;
}

/// web_fetch 工具：策略在此，网络在 Fetcher 实现。
pub struct WebFetchTool {
    fetcher: Arc<dyn Fetcher>,
    allowed_hosts: BTreeSet<String>,
    spill_root: PathBuf,
    locator_base: String,
    spill_counter: AtomicU64,
    max_bytes: usize,
}

impl WebFetchTool {
    /// `locator_base` 是 spill 文件相对工作区的目录前缀（如 `.harness/spill`），
    /// 使 locator 可以直接被 fs_read 取回。
    pub fn new(
        fetcher: Arc<dyn Fetcher>,
        allowed_hosts: BTreeSet<String>,
        spill_root: PathBuf,
        locator_base: &str,
        max_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            fetcher,
            allowed_hosts,
            spill_root,
            locator_base: locator_base.trim_matches('/').to_string(),
            spill_counter: AtomicU64::new(0),
            max_bytes,
        })
    }

    pub fn allowed_hosts(&self) -> &BTreeSet<String> {
        &self.allowed_hosts
    }

    /// 主入口：解析 → SSRF/白名单 → 取回（重定向逐跳复检）→ 解码/截断。
    pub fn fetch(&self, args: &Value) -> protocol::ToolOutcome {
        match self.fetch_inner(args) {
            Ok(value) => protocol::ToolOutcome::Success { value },
            Err(error) => protocol::ToolOutcome::Failure { error },
        }
    }

    fn fetch_inner(&self, args: &Value) -> Result<Value, ErrorEnvelope> {
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return Err(args_error("missing string argument: url"));
        };
        let mut current = url.trim().to_string();
        for _hop in 0..=MAX_REDIRECT_HOPS {
            let (scheme, host) = parse_url(&current)?;
            if is_private_host(&host) {
                return Err(denied(format!(
                    "private or loopback hosts are not fetchable: {host}"
                )));
            }
            if !self.allowed_hosts.contains(&host) {
                return Err(denied(format!(
                    "host is not allowlisted for web_fetch: {host}"
                )));
            }
            let response = self.fetcher.fetch(&FetchRequest {
                url: current.clone(),
                max_bytes: self.max_bytes,
            })?;
            if (300..400).contains(&response.status) {
                let Some(location) = response.location else {
                    return Err(args_error(format!(
                        "redirect status {} without Location header",
                        response.status
                    )));
                };
                let next = location.trim().to_string();
                let (next_scheme, _) = parse_url(&next)?;
                if next_scheme != "http" && next_scheme != "https" {
                    return Err(denied(format!(
                        "redirect to non-http scheme is not fetchable: {next}"
                    )));
                }
                current = next;
                continue;
            }
            if response.truncated {
                let text = decode_body(&response.body)?;
                let locator = self.spill(&text)?;
                return Ok(serde_json::json!({
                    "url": current,
                    "status": response.status,
                    "content": preview(&text),
                    "truncated": true,
                    "locator": locator,
                }));
            }
            let text = decode_body(&response.body)?;
            if text.chars().count() > MAX_CONTENT_CHARS {
                let locator = self.spill(&text)?;
                return Ok(serde_json::json!({
                    "url": current,
                    "status": response.status,
                    "content": preview(&text),
                    "truncated": true,
                    "locator": locator,
                }));
            }
            return Ok(serde_json::json!({
                "url": current,
                "status": response.status,
                "content": text,
                "truncated": false,
            }));
        }
        Err(args_error(format!(
            "too many redirects (>{MAX_REDIRECT_HOPS})"
        )))
    }

    fn spill(&self, content: &str) -> Result<String, ErrorEnvelope> {
        if content.len() > MAX_SPILL_BYTES {
            return Err(args_error("fetched content exceeds spill size limit"));
        }
        fs::create_dir_all(&self.spill_root)
            .map_err(|error| io_error("create spill directory", error))?;
        let name = format!(
            "web-{}-{}.txt",
            std::process::id(),
            self.spill_counter.fetch_add(1, Ordering::Relaxed)
        );
        let path = self.spill_root.join(&name);
        let mut file = fs::File::create(&path).map_err(|error| path_error(&path, error))?;
        file.write_all(content.as_bytes())
            .map_err(|error| path_error(&path, error))?;
        Ok(format!("{}/{}", self.locator_base, name))
    }
}

/// 极简 URL 解析：scheme://host[:port][/path]；host 保留 IPv6 方括号内形式。
fn parse_url(url: &str) -> Result<(String, String), ErrorEnvelope> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| args_error(format!("url must include a scheme: {url}")))?;
    if scheme != "http" && scheme != "https" {
        return Err(denied(format!("only http/https urls are fetchable: {url}")));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(args_error(format!("url has no host: {url}")));
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        let inner = stripped
            .split(']')
            .next()
            .ok_or_else(|| args_error(format!("malformed IPv6 host: {url}")))?;
        format!("[{inner}]")
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    if host.is_empty() {
        return Err(args_error(format!("url has no host: {url}")));
    }
    Ok((scheme.to_string(), host.to_ascii_lowercase()))
}

/// 私网/回环主机判定（SSRF 第一道闸；DNS 级钉扎由物理通道与代理负责）。
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return is_private_ipv6(inner);
    }
    if let Some(ip) = parse_ipv4(host) {
        let [a, b, _, _] = ip;
        return matches!(
            (a, b),
            (0, _)
                | (10, _)
                | (127, _)
                | (169, 254)
                | (172, 16..=31)
                | (192, 168)
                | (100, 64..=127)
        );
    }
    false
}

fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        octets[index] = part.parse::<u8>().ok()?;
    }
    Some(octets)
}

fn is_private_ipv6(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "::1" || h == "::" {
        return true;
    }
    if let Some(mapped) = h.strip_prefix("::ffff:") {
        if let Some(ip) = parse_ipv4(mapped) {
            return is_private_host(&format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]));
        }
    }
    h.starts_with("fc") || h.starts_with("fd") || h.starts_with("fe80")
}

fn decode_body(body: &[u8]) -> Result<String, ErrorEnvelope> {
    if body.iter().any(|byte| *byte == 0) {
        return Err(args_error(
            "fetched content is binary; only text is supported",
        ));
    }
    Ok(String::from_utf8_lossy(body).into_owned())
}

fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn denied(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::SandboxDenied, message)
}

fn io_error(action: impl AsRef<str>, error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to {}: {error}", action.as_ref()),
    )
}

fn path_error(path: &PathBuf, error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to access {}: {error}", path.display()),
    )
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
