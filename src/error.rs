//! A2 · 错误类型（第二版 §2.3、§2.12）。
//!
//! 顶层一律 `anyhow::Result`；provider 错误是独立枚举，
//! `16` 的 loop 里只匹配 `ContextOverflow` 与 `RateLimited`。

use std::time::Duration;

pub use anyhow::Result;
use thiserror::Error;

/// provider 层错误（第二版 §2.3）。
#[derive(Debug, Error)]
pub enum ProviderError {
    /// 429：`retry_after` 取 Retry-After 头 / body 的 retry_after_seconds（封顶后）。
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },
    /// 400 且错误文案命中各家溢出提示（判定集中在 `provider::http::is_context_overflow`）。
    #[error("context overflow")]
    ContextOverflow,
    /// 401 / 403 等鉴权失败。
    #[error("authentication failed")]
    Auth,
    /// 其他 HTTP 错误：状态码 + 响应体摘要。
    #[error("http error {0}: {1}")]
    Http(u16, String),
    /// 连接 / IO / SSE 中断等传输错误。
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_display_and_anyhow_compatible() {
        let e = ProviderError::Http(500, "boom".into());
        assert_eq!(e.to_string(), "http error 500: boom");
        let err: anyhow::Error = ProviderError::ContextOverflow.into();
        assert_eq!(err.to_string(), "context overflow");
        assert!(matches!(
            ProviderError::RateLimited { retry_after: None },
            ProviderError::RateLimited { .. }
        ));
    }
}
