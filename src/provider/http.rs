//! HTTP / SSE / 重试层（第二版 §2.3 http.rs）。
//!
//! TODO(08)：填实现。SSE 按空行切事件、取 `data:` 行、`[DONE]` 结束（约 80 行，
//! 不引 eventsource 库）；重试参数见下方常量（goose
//! `goose-provider-types/src/retry.rs:8~11`）；`Retry-After` 与 body 的
//! `retry_after_seconds` 优先且封顶（搬 goose `goose-providers/src/http_status.rs:47~83`
//! 时注明出处）。

use std::collections::BTreeMap;
use std::time::Duration;

use futures::stream::BoxStream;
use serde_json::Value;

use crate::error::ProviderError;

/// 429 / 500 / 502 / 503 / 529 重试（goose retry.rs）。
pub const RETRYABLE_STATUSES: [u16; 5] = [429, 500, 502, 503, 529];
pub const MAX_RETRIES: u32 = 3;
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const BACKOFF_FACTOR: u32 = 2;
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 一条 SSE 事件（`event:` 行可缺省，`data:` 行聚合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 增量解析器：跨网络块缓冲，按空行切事件。TODO(08)
#[derive(Debug, Default)]
pub struct SseParser {
    pub buffer: String,
}

impl SseParser {
    pub fn feed(&mut self, _chunk: &str) -> Vec<SseEvent> {
        todo!("TODO(08)")
    }
}

/// `data: [DONE]` 结束标记。TODO(08)
pub fn is_done(_ev: &SseEvent) -> bool {
    todo!("TODO(08)")
}

/// 共享 reqwest client（超时默认 [`crate::provider::DEFAULT_PROVIDER_TIMEOUT`]）。
#[derive(Debug, Clone)]
pub struct HttpClient {
    pub inner: reqwest::Client,
    pub timeout: Duration,
}

impl HttpClient {
    pub fn new(_timeout: Duration) -> crate::Result<Self> {
        todo!("TODO(08)")
    }

    /// POST JSON 拿流式响应 → SSE 事件流；含退避重试与 Retry-After 优先。TODO(08)
    pub async fn post_sse(
        &self,
        _url: &str,
        _headers: &BTreeMap<String, String>,
        _body: &Value,
    ) -> crate::Result<BoxStream<'static, crate::Result<SseEvent>>> {
        todo!("TODO(08)")
    }
}

/// 状态码 + 响应体 → [`ProviderError`]（429 带 retry_after；401/403 → Auth）。TODO(08)
pub fn map_http_error(_status: u16, _body: &str) -> ProviderError {
    todo!("TODO(08)")
}

/// 400 且文案含 "prompt is too long" / "context_length_exceeded" /
/// "maximum context length" → ContextOverflow（各家文案匹配集中在一个函数）。TODO(08)
pub fn is_context_overflow(_status: u16, _text: &str) -> bool {
    todo!("TODO(08)")
}

/// `Retry-After` 头（秒 / HTTP-date）与 body `retry_after_seconds`，封顶后返回。TODO(08)
pub fn extract_retry_after(
    _headers: &reqwest::header::HeaderMap,
    _body: &Value,
) -> Option<Duration> {
    todo!("TODO(08)")
}
