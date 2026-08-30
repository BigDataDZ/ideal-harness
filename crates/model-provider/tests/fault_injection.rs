//! TASK-102 验收测试：本地 `TcpListener` 手写 HTTP 响应的故障注入。
//!
//! 覆盖任务卡三种故障：① 超时 ② 截断（缺 [DONE]）③ 非 JSON data 行；
//! 另加正常路径聚合与半途断连，锁住 IO 分支的行为。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use model_provider::{ChatMessage, ChatModel, OpenAiCompatClient};
use protocol::{ErrorCode, ModelCallSpec};
use std::net::TcpListener;

const SHORT_TIMEOUT: Duration = Duration::from_millis(300);

fn spec(base_url: &str) -> ModelCallSpec {
    ModelCallSpec {
        model: "mock-model".into(),
        base_url: base_url.to_string(),
        temperature: None,
    }
}

fn client() -> OpenAiCompatClient {
    OpenAiCompatClient::with_key_for_loopback_test("test-key", SHORT_TIMEOUT).unwrap()
}

/// 启动一次性 mock server：接受单个连接、排空请求头+体后执行 handler。
fn spawn_mock(handler: impl FnOnce(TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 127.0.0.1 失败");
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept 失败");
        drain_request(&stream);
        handler(stream);
    });
    format!("http://{addr}")
}

/// 读掉请求头直到空行；若有 Content-Length 则把请求体也读净，
/// 避免客户端写阻塞。
fn drain_request(stream: &TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return; // 对端已关闭
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        let _ = reader.read_exact(&mut buf);
    }
}

fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

use std::thread;

#[test]
fn happy_path_aggregates_deltas_and_finish_reason() {
    let body = concat!(
        r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":"你好"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":", world"},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        r#"data: {"choices":[],"usage":{"total_tokens":37}}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let base = spawn_mock(move |mut stream| {
        let _ = stream.write_all(sse_response(body).as_bytes());
        let _ = stream.flush();
    });

    let reply = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect("正常路径不应失败");
    assert_eq!(reply.text, "你好, world");
    assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    assert_eq!(reply.usage.unwrap().total_tokens, 37);
}

/// 故障注入 ①：上游接受连接后挂起不响应——300ms 客户端超时必须触发，
/// 映射为 ModelStreamBroken，且不能傻等 mock 的 5s 才返回。
#[test]
fn hung_upstream_times_out_to_model_stream_broken() {
    let started = Instant::now();
    let base = spawn_mock(|_stream| {
        // 故意不写响应也不关闭：模拟上游挂起
        thread::sleep(Duration::from_secs(5));
    });

    let err = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect_err("挂起的上游必须以超时失败");
    let elapsed = started.elapsed();

    assert_eq!(err.code, ErrorCode::ModelStreamBroken, "实际错误: {err:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "应在客户端超时附近返回而非等满服务端 5s；实际 {elapsed:?}"
    );
}

/// 故障注入 ②：HTTP 层完整（Content-Length 精确）但 SSE 缺 [DONE] 哨兵
/// ——必须显式报截断，不得把残缺回复当完整回复。
#[test]
fn stream_without_done_sentinel_is_reported_as_truncated() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"半截\"}}]}\n\n".to_string();
    let base = spawn_mock(move |mut stream| {
        let _ = stream.write_all(sse_response(&body).as_bytes());
        let _ = stream.flush();
    });

    let err = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect_err("缺 [DONE] 必须报截断");
    assert_eq!(err.code, ErrorCode::ModelStreamBroken);
    assert!(
        err.message.contains("[DONE]"),
        "message 应说明截断原因: {err:?}"
    );
}

/// 故障注入 ③：data 行不是合法 chunk JSON——必须报 ModelStreamBroken，
/// 禁止静默跳过掩盖协议漂移。
#[test]
fn non_json_data_line_is_model_stream_broken_not_skipped() {
    let body = "data: <html>bad gateway</html>\n\ndata: [DONE]\n\n";
    let base = spawn_mock(move |mut stream| {
        let _ = stream.write_all(sse_response(body).as_bytes());
        let _ = stream.flush();
    });

    let err = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect_err("非 JSON data 行必须报错");
    assert_eq!(err.code, ErrorCode::ModelStreamBroken);
}

/// 附加注入：声明 Content-Length 却中途断开连接——读阶段 IO 错误同样映射
/// ModelStreamBroken（与语义层截断互补，覆盖传输分支）。
#[test]
fn connection_cut_mid_body_is_model_stream_broken() {
    let base = spawn_mock(|mut stream| {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Content-Length: 100\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(b"data: {\"choi"); // 少于声明的 100 字节即断
        let _ = stream.flush();
        drop(stream); // 关闭写端 → 对端读到 EOF 但字节数不足
    });

    let err = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect_err("半途断连必须报流中断");
    assert_eq!(err.code, ErrorCode::ModelStreamBroken);
}

/// 工具调用闭环（TASK-103 前置）：tool_calls 分片跨多个 chunk 到达，
/// 必须按 index 重组——id/name 取首见值，arguments 按序拼接。
#[test]
fn tool_call_fragments_are_aggregated_by_index() {
    let body = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"echo","arguments":"{\"text\":"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hi\"}"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let base = spawn_mock(move |mut stream| {
        let _ = stream.write_all(sse_response(body).as_bytes());
        let _ = stream.flush();
    });

    let reply = client()
        .stream_chat(&spec(&base), &[ChatMessage::user("hi")], None)
        .expect("工具调用流不应失败");
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(reply.tool_calls.len(), 1);
    let tc = &reply.tool_calls[0];
    assert_eq!(tc.id, "call_9");
    assert_eq!(tc.name, "echo");
    assert_eq!(tc.arguments, r#"{"text":"hi"}"#);
}
