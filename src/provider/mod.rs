//! provider 层（第二版 §2.3；第三版 §2.4：provider 定义来自插件，内核只有引擎）。
//!
//! 类型布局由 00 锁定；`context_limit_for` 由 08 实现，
//! engine 实现在 [`openai`] / [`proxy`] / [`anthropic`]，装配在 [`registry`]。

#[cfg(feature = "anthropic-engine")]
pub mod anthropic;
pub mod http;
pub mod openai;
pub mod proxy;
pub mod registry;
mod shared;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde::Serialize;

use crate::error::ProviderError;
use crate::message::Message;
use crate::message::Usage;
use crate::tools::ToolSpec;

pub use registry::ProviderRegistry;

/// 兜底上下文上限（第二版 §2.3）。
pub const DEFAULT_CONTEXT_LIMIT: u32 = 128 * 1024;
/// goose DEFAULT_PROVIDER_TIMEOUT_SECS。
pub const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

/// provider 流式事件；`stream_assistant`（`16`）把它折叠成一条 assistant Message。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    /// JSON 片段，累积到 [`StreamEvent::ToolUseEnd`] 再 parse。
    ToolUseDelta(String),
    ToolUseEnd,
    Done {
        usage: Usage,
        stop_reason: StopReason,
    },
}

/// 请求参数全借用，loop 侧零拷贝。
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

/// 模型名前缀小表 + 兜底 128k：claude 200k、gpt-4o 128k、gpt-4.1 1M、
/// o 系列 200k、deepseek 128k、llama 128k（第二版 §2.3）。
/// 名字带 `provider/` 命名空间前缀时按最后一段匹配。
pub fn context_limit_for(model: &str) -> u32 {
    const K: u32 = 1024;
    let m = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    if m.starts_with("claude") {
        200 * K
    } else if m.starts_with("gpt-4.1") {
        1024 * K
    } else if m.starts_with("gpt-4o") {
        128 * K
    } else if is_o_series(&m) {
        200 * K
    } else {
        // deepseek / llama 未列出的其余模型一律兜底 128k。
        DEFAULT_CONTEXT_LIMIT
    }
}

/// o 系列 = 首字符 `o` + 数字（o1 / o3 / o4-mini …）。
fn is_o_series(m: &str) -> bool {
    let mut chars = m.chars();
    matches!(chars.next(), Some('o')) && chars.next().is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_limit_prefix_table() {
        const K: u32 = 1024;
        assert_eq!(context_limit_for("claude-sonnet-4-5"), 200 * K);
        assert_eq!(context_limit_for("gpt-4o-mini"), 128 * K);
        assert_eq!(context_limit_for("gpt-4.1-nano"), 1024 * K);
        assert_eq!(context_limit_for("o3"), 200 * K);
        assert_eq!(context_limit_for("o1-preview"), 200 * K);
        assert_eq!(context_limit_for("deepseek-chat"), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for("llama3.1-70b"), DEFAULT_CONTEXT_LIMIT);
    }

    #[test]
    fn context_limit_fallback_and_normalization() {
        assert_eq!(context_limit_for("mistral-large"), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for(""), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for("openai/gpt-4o"), 128 * 1024);
        assert_eq!(context_limit_for("GPT-4.1"), 1024 * 1024);
        // "ollama" 之类 o 开头但非 o+数字，不误判为 o 系列。
        assert_eq!(context_limit_for("ollama"), DEFAULT_CONTEXT_LIMIT);
    }
}

/// `dev.instagent/providers/*.json` 的形状（第三版 §2.4；沿用 goose
/// DeclarativeProviderConfig，去 setup 向导字段，加 `proxy`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDef {
    pub name: String,
    pub engine: EngineKind,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// 密钥只能走环境变量（第三版 §2.10）。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// openai 引擎写到 `/v1`，请求时拼 `/chat/completions`（转换脚本处理，见 `10`）。
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub models: Vec<ModelDef>,
    /// engine = proxy 时必填，拉起逻辑在 `11`。
    #[serde(default)]
    pub proxy: Option<ProxyDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Openai,
    Proxy,
    /// cargo feature `anthropic-engine`（`12`）。
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDef {
    pub name: String,
    #[serde(default)]
    pub context_limit: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// `engine: "proxy"` 的拉起配置（第三版 §2.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyDef {
    /// 单个可执行名或 `./` 开头的插件相对路径。
    pub command: String,
    /// `${PORT}` 在拉起时替换（`11`）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 就绪探针路径，默认 [`ProxyDef::DEFAULT_READY`]。
    #[serde(default)]
    pub ready: Option<String>,
    /// 就绪超时秒数，默认 [`ProxyDef::DEFAULT_TIMEOUT_SECS`]。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ProxyDef {
    pub const DEFAULT_READY: &'static str = "/v1/models";
    pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
}
