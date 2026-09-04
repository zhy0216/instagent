//! HTTP / SSE / 重试层（第二版 §2.3 http.rs）。
//!
//! SSE 按空行切事件、取 `data:` 行、`[DONE]` 结束（hand-roll，不引 eventsource 库）；
//! 重试参数见下方常量（goose `goose-provider-types/src/retry.rs:8~11`）；
//! `Retry-After` 与 body 的 `retry_after_seconds` 优先且封顶
//! （移植 goose `goose-providers/src/http_status.rs:47~83`，出处另见 commit message）。

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::time::Duration;

use futures::stream;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use reqwest::header::ACCEPT;
use reqwest::header::RETRY_AFTER;
use serde_json::Value;

use crate::error::ProviderError;

/// 429 / 500 / 502 / 503 / 529 重试（goose retry.rs）。
const RETRYABLE_STATUSES: [u16; 5] = [429, 500, 502, 503, 529];
pub const MAX_RETRIES: u32 = 3;
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const BACKOFF_FACTOR: u32 = 2;
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 远端提示的重试等待硬封顶（goose `http_status.rs` MAX_RETRY_AFTER_SECS：
/// 畸形的大值降级为指数退避，而不是把 agent 冻住）。
const MAX_RETRY_AFTER_SECS: f64 = 3600.0;

/// [`HttpClient`] 的退避策略；生产默认见上方常量，测试可注入小值。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub factor: u32,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            initial_backoff: INITIAL_BACKOFF,
            factor: BACKOFF_FACTOR,
            cap: BACKOFF_CAP,
        }
    }
}

/// 第 `attempt` 次（0 起）失败后的指数退避时长，封顶 `cap`。
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let mut delay = policy.initial_backoff;
    for _ in 0..attempt {
        delay = delay.saturating_mul(policy.factor);
    }
    delay.min(policy.cap)
}

/// 一条 SSE 事件（`event:` 行可缺省，`data:` 行聚合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 增量解析器：跨网络块缓冲，按空行切事件。
#[derive(Debug, Default)]
pub struct SseParser {
    pub buffer: String,
}

impl SseParser {
    /// 喂入一个网络块，吐出其中完整的事件；不完整块留在缓冲区。
    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        if self.buffer.contains('\r') {
            self.buffer = self.buffer.replace("\r\n", "\n");
        }
        let mut events = Vec::new();
        while let Some(idx) = self.buffer.find("\n\n") {
            let block = self.buffer[..idx].to_owned();
            self.buffer.drain(..idx + 2);
            if let Some(ev) = parse_event_block(&block) {
                events.push(ev);
            }
        }
        events
    }
}

/// 解析单个事件块：`data:` 行按 `\n` 聚合，`event:` 取最后一次，
/// 其余字段（`id:` / `retry:` / `:` 注释）忽略。全空块返回 None。
fn parse_event_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
        }
    }
    if data_lines.is_empty() && event.is_none() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

/// `data: [DONE]` 结束标记。
pub fn is_done(ev: &SseEvent) -> bool {
    ev.data.trim() == "[DONE]"
}

/// 共享 reqwest client（超时默认 [`crate::provider::DEFAULT_PROVIDER_TIMEOUT`]）。
#[derive(Debug, Clone)]
pub struct HttpClient {
    pub inner: reqwest::Client,
    pub timeout: Duration,
    pub retry: RetryPolicy,
}

impl HttpClient {
    pub fn new(timeout: Duration) -> crate::Result<Self> {
        let inner = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            inner,
            timeout,
            retry: RetryPolicy::default(),
        })
    }

    /// 覆盖退避策略（测试用小值，避免 1s 首退拖慢 wiremock 用例）。
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// POST JSON 拿流式响应 → SSE 事件流；含退避重试与 Retry-After 优先。
    pub async fn post_sse(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> crate::Result<BoxStream<'static, crate::Result<SseEvent>>> {
        let mut attempt: u32 = 0;
        loop {
            let resp = self
                .inner
                .post(url)
                .headers(build_headers(headers))
                .json(body)
                .send()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            let status = resp.status().as_u16();
            if !RETRYABLE_STATUSES.contains(&status) {
                if resp.status().is_success() {
                    return Ok(event_stream(resp));
                }
                let text = read_body(resp).await;
                return Err(map_http_error(status, &text).into());
            }
            if attempt >= self.retry.max_retries {
                let text = read_body(resp).await;
                return Err(map_http_error(status, &text).into());
            }
            // 429：Retry-After / body 提示优先于指数退避。
            let delay = if status == 429 {
                let hint_headers = resp.headers().clone();
                let text = read_body(resp).await;
                extract_retry_after(&hint_headers, &parse_body_json(&text))
            } else {
                None
            }
            .unwrap_or_else(|| backoff_delay(&self.retry, attempt));
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// 调用方头 + SSE `Accept`；非法头名/值跳过（交给服务端报错）。
fn build_headers(headers: &BTreeMap<String, String>) -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// HTTP 错误 body 的读取上限（todo 08 / R9）：错误 body 只用于 500 字符
/// 摘要与重试提示，超限部分直接丢弃，坏响应不能撑爆内存。
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// 有界读错误 body：按块累积到 [`MAX_ERROR_BODY_BYTES`] 即停。
async fn read_body(resp: reqwest::Response) -> String {
    let mut stream = resp.bytes_stream();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() >= MAX_ERROR_BODY_BYTES {
                    bytes.truncate(MAX_ERROR_BODY_BYTES);
                    break;
                }
            }
            // 传输中断：已读部分仍可用于摘要 / 重试提示。
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_body_json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// 字节流 → SSE 事件流：跨块喂 [`SseParser`]，传输错误以
/// [`ProviderError::Transport`] 作为最后一个元素冒泡。
fn event_stream(resp: reqwest::Response) -> BoxStream<'static, crate::Result<SseEvent>> {
    struct State {
        chunks: BoxStream<'static, crate::Result<String>>,
        parser: SseParser,
        pending: VecDeque<SseEvent>,
        finished: bool,
    }
    let chunks: BoxStream<'static, crate::Result<String>> = resp
        .bytes_stream()
        .map(|chunk| {
            chunk
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|e| anyhow::Error::new(ProviderError::Transport(e.to_string())))
        })
        .boxed();
    let state = State {
        chunks,
        parser: SseParser::default(),
        pending: VecDeque::new(),
        finished: false,
    };
    stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((Ok(ev), st));
            }
            if st.finished {
                return None;
            }
            match st.chunks.next().await {
                Some(Ok(text)) => st.pending.extend(st.parser.feed(&text)),
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
                None => st.finished = true,
            }
        }
    })
    .boxed()
}

/// 错误摘要长度上限（字符数）。
pub const ERROR_SUMMARY_CHARS: usize = 500;

/// 状态码 + 响应体 → [`ProviderError`]（429 带 retry_after；401/403 → Auth）。
/// 摘要先截断再对 `sk-…` 形态做 redact（ADR 0003 D1：错误输出不带原始密钥）。
pub fn map_http_error(status: u16, body: &str) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited {
            retry_after: extract_retry_after(&HeaderMap::new(), &parse_body_json(body)),
        },
        _ if is_context_overflow(status, body) => ProviderError::ContextOverflow,
        _ => ProviderError::Http(
            status,
            redact_secret_tokens(&summarize(body, ERROR_SUMMARY_CHARS)),
        ),
    }
}

/// 文本摘要：按 char 边界截到 `max` 个字符（错误消息有界，todo 08 / S18）。
pub(crate) fn summarize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect()
}

/// redact `sk-…` 形态的密钥（ADR 0003 D1）：`sk-` 后跟随 >= 8 个
/// `[A-Za-z0-9_-]` 且前面不是字母数字（避免误伤 `mask-…` 之类词）时替换为
/// `sk-[redacted]`。
pub(crate) fn redact_secret_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    while let Some(rel) = text[start..].find("sk-") {
        let idx = start + rel;
        let word_char_before = text[..idx]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let tail = &text[idx + 3..];
        let run: usize = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .map(char::len_utf8)
            .sum();
        if !word_char_before && run >= 8 {
            out.push_str(&text[start..idx]);
            out.push_str("sk-[redacted]");
        } else {
            out.push_str(&text[start..idx + 3 + run]);
        }
        start = idx + 3 + run;
    }
    out.push_str(&text[start..]);
    out
}

/// 400 且文案含 "prompt is too long" / "context_length_exceeded" /
/// "maximum context length" → ContextOverflow（各家文案匹配集中在本函数）。
pub fn is_context_overflow(status: u16, text: &str) -> bool {
    if status != 400 {
        return false;
    }
    const PHRASES: [&str; 3] = [
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
    ];
    let lower = text.to_lowercase();
    PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// 从 429 响应提取重试等待：优先 body 的
/// `error.metadata.retry_after_seconds`（OpenRouter 形状，比整数头精确），
/// 回落到 RFC 7231 的 `Retry-After` 头（秒或 HTTP-date），统一封顶。
/// （移植 goose `goose-providers/src/http_status.rs:47~83`）
pub fn extract_retry_after(headers: &HeaderMap, body: &Value) -> Option<Duration> {
    if let Some(secs) = body
        .get("error")
        .and_then(|e| e.get("metadata"))
        .and_then(|m| m.get("retry_after_seconds"))
        .and_then(Value::as_f64)
    {
        if let Some(d) = duration_from_finite_secs(secs) {
            return Some(d);
        }
    }
    headers
        .get(RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| parse_retry_after_header(s.trim()))
}

/// 有限、非负、范围内的秒数 → `Duration`；NaN / 负数 / 无穷 / 超大输入返回
/// `None`（`Duration::from_secs_f64` 对后者会 panic）。封顶 1 小时。
fn duration_from_finite_secs(secs: f64) -> Option<Duration> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER_SECS)))
}

/// `Retry-After`：只认非负整数秒（OpenAI / OpenRouter 实际都发
/// 整数秒；HTTP-date 形式不解析，退回指数退避）。
fn parse_retry_after_header(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|secs| duration_from_finite_secs(secs as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_retries: MAX_RETRIES,
            initial_backoff: Duration::from_millis(1),
            factor: BACKOFF_FACTOR,
            cap: Duration::from_millis(20),
        }
    }

    fn test_client() -> HttpClient {
        HttpClient::new(Duration::from_secs(5))
            .unwrap()
            .with_retry(fast_retry())
    }

    // ---- SSE 解析 ----

    #[test]
    fn sse_parses_multiple_events_and_splits_across_chunks() {
        let mut parser = SseParser::default();
        let mut events = parser.feed("data: {\"a\":1}\n\ndata: {\"b\"");
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "{\"a\":1}".into()
            }]
        );
        events.extend(parser.feed(":2}\n\nevent: ping\ndata: {}\n\n"));
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "{\"b\":2}".into()
                },
                SseEvent {
                    event: Some("ping".into()),
                    data: "{}".into()
                },
            ]
        );
        // 残留：不完整的最后一个块不吐出。
        let mut tail = SseParser::default();
        assert!(tail.feed("data: partial").is_empty());
        assert_eq!(tail.buffer, "data: partial");
    }

    #[test]
    fn sse_multi_line_data_and_crlf_and_comments() {
        let mut parser = SseParser::default();
        let events = parser.feed(": keepalive\r\ndata: line1\r\ndata: line2\r\n\r\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "line1\nline2".into()
            }]
        );
    }

    #[test]
    fn sse_done_marker() {
        assert!(is_done(&SseEvent {
            event: None,
            data: "[DONE]".into()
        }));
        assert!(is_done(&SseEvent {
            event: None,
            data: "[DONE] ".into()
        }));
        assert!(!is_done(&SseEvent {
            event: None,
            data: "x".into()
        }));
    }

    #[test]
    fn backoff_is_exponential_capped_and_policy_defaults_match_consts() {
        let policy = RetryPolicy::default();
        assert_eq!(backoff_delay(&policy, 0), Duration::from_secs(1));
        assert_eq!(backoff_delay(&policy, 1), Duration::from_secs(2));
        assert_eq!(backoff_delay(&policy, 5), Duration::from_secs(30));
    }

    // ---- Retry-After 提取 ----

    #[test]
    fn retry_after_prefers_body_metadata_over_header() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("10"));
        let body: Value =
            serde_json::from_str(r#"{"error":{"metadata":{"retry_after_seconds":1.5}}}"#).unwrap();
        assert_eq!(
            extract_retry_after(&headers, &body),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn retry_after_header_seconds_parsed_and_capped() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(
            extract_retry_after(&headers, &Value::Null),
            Some(Duration::from_secs(2))
        );

        // 非整数秒（含 HTTP-date）不解析：退回指数退避。
        headers.insert(RETRY_AFTER, HeaderValue::from_static("1e30"));
        assert_eq!(extract_retry_after(&headers, &Value::Null), None);
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        assert_eq!(extract_retry_after(&headers, &Value::Null), None);

        headers.insert(RETRY_AFTER, HeaderValue::from_static("99999999999999"));
        assert_eq!(
            extract_retry_after(&headers, &Value::Null),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn retry_after_body_absurd_is_capped_and_negative_degrades_to_header() {
        let body: Value =
            serde_json::from_str(r#"{"error":{"metadata":{"retry_after_seconds":1e30}}}"#).unwrap();
        assert_eq!(
            extract_retry_after(&HeaderMap::new(), &body),
            Some(Duration::from_secs(3600))
        );
        // 负数是坏提示：不直接返回，回落到 Retry-After 头。
        let body: Value =
            serde_json::from_str(r#"{"error":{"metadata":{"retry_after_seconds":-3}}}"#).unwrap();
        assert_eq!(extract_retry_after(&HeaderMap::new(), &body), None);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(
            extract_retry_after(&headers, &body),
            Some(Duration::from_secs(7))
        );
    }

    // ---- 状态码映射与 ContextOverflow 判定 ----

    #[test]
    fn maps_auth_rate_limited_and_http() {
        assert!(matches!(map_http_error(401, ""), ProviderError::Auth));
        assert!(matches!(map_http_error(403, ""), ProviderError::Auth));
        let err = map_http_error(429, r#"{"error":{"metadata":{"retry_after_seconds":3}}}"#);
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(3)
        ));
        let err = map_http_error(500, "boom");
        assert!(matches!(err, ProviderError::Http(500, ref m) if m == "boom"));
    }

    #[test]
    fn summarize_truncates_on_char_boundary() {
        assert_eq!(summarize("boom", ERROR_SUMMARY_CHARS), "boom");
        assert_eq!(summarize(&"x".repeat(600), ERROR_SUMMARY_CHARS).len(), 500);
        // 多字节字符按字符数截，不产生半个字符。
        assert_eq!(summarize("中文中文中文", 3), "中文中");
    }

    #[test]
    fn redact_secret_tokens_masks_sk_shaped_keys() {
        assert_eq!(
            redact_secret_tokens("invalid api key sk-abcdef12345 provided"),
            "invalid api key sk-[redacted] provided"
        );
        // 短于 8 个跟随字符的不算密钥形态。
        assert_eq!(redact_secret_tokens("sk-short"), "sk-short");
        // 词中出现的 `sk-` 不误伤（mask-… / task-…）。
        assert_eq!(
            redact_secret_tokens("mask-12345678 done"),
            "mask-12345678 done"
        );
        // Bearer 头形态。
        assert_eq!(
            redact_secret_tokens("Bearer sk-AAAAAAAAAAAAAAAA"),
            "Bearer sk-[redacted]"
        );
    }

    #[test]
    fn context_overflow_detection_centered_in_one_fn() {
        for phrase in [
            "prompt is too long: 200001 tokens",
            "This model's maximum context length is 8192 tokens",
            r#"{"error":{"code":"context_length_exceeded"}}"#,
        ] {
            assert!(is_context_overflow(400, phrase), "{phrase}");
            assert!(matches!(
                map_http_error(400, phrase),
                ProviderError::ContextOverflow
            ));
        }
        assert!(!is_context_overflow(400, "invalid api key"));
        assert!(!is_context_overflow(429, "prompt is too long"));
    }

    // ---- wiremock 集成 ----

    async fn collect_until_done(
        stream: &mut BoxStream<'static, crate::Result<SseEvent>>,
    ) -> crate::Result<Vec<SseEvent>> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev?;
            let done = is_done(&ev);
            out.push(ev);
            if done {
                break;
            }
        }
        Ok(out)
    }

    #[tokio::test]
    async fn post_sse_retries_429_with_retry_after_then_streams() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(r#"{"error":{"message":"slow down"}}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            ))
            .mount(&server)
            .await;

        let mut stream = test_client()
            .post_sse(
                &format!("{}/v1/chat/completions", server.uri()),
                &BTreeMap::from([("authorization".to_string(), "Bearer x".to_string())]),
                &serde_json::json!({"model": "gpt-4o"}),
            )
            .await
            .expect("first 429 retried");
        let events = collect_until_done(&mut stream).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].data.contains("hi"));
        assert!(is_done(&events[1]));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn post_sse_exhausts_retries_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(4) // 1 次初始 + MAX_RETRIES=3 次重试
            .mount(&server)
            .await;

        let err = test_client()
            .post_sse(
                &format!("{}/v1/chat/completions", server.uri()),
                &BTreeMap::new(),
                &serde_json::json!({}),
            )
            .await
            .map(drop)
            .expect_err("500 should fail after retries");
        let pe = err.downcast_ref::<ProviderError>().unwrap();
        assert!(matches!(pe, ProviderError::Http(500, m) if m == "boom"));
    }

    #[tokio::test]
    async fn post_sse_maps_400_overflow_to_context_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
            ))
            .mount(&server)
            .await;

        let err = test_client()
            .post_sse(
                &format!("{}/v1/chat/completions", server.uri()),
                &BTreeMap::new(),
                &serde_json::json!({}),
            )
            .await
            .map(drop)
            .expect_err("400 overflow");
        assert!(matches!(
            err.downcast_ref::<ProviderError>().unwrap(),
            ProviderError::ContextOverflow
        ));
        // 400 不在重试集合内：只发一次请求。
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn post_sse_error_body_is_bounded_and_redacted() {
        let server = MockServer::start().await;
        // 超大错误 body 里夹带密钥形态：读取有界、摘要有界、密钥被 redact。
        let mut body = String::from("boom sk-topsecretkey99 ");
        body.push_str(&"x".repeat(MAX_ERROR_BODY_BYTES * 2));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string(body))
            .mount(&server)
            .await;

        let err = test_client()
            .post_sse(
                &format!("{}/v1/chat/completions", server.uri()),
                &BTreeMap::new(),
                &serde_json::json!({}),
            )
            .await
            .map(drop)
            .unwrap_err();
        let pe = err.downcast_ref::<ProviderError>().unwrap();
        let ProviderError::Http(500, message) = pe else {
            panic!("expected Http(500), got {pe:?}")
        };
        assert!(message.chars().count() <= ERROR_SUMMARY_CHARS, "{message}");
        assert!(message.contains("sk-[redacted]"), "{message}");
        assert!(!message.contains("sk-topsecretkey99"), "{message}");
    }
}
