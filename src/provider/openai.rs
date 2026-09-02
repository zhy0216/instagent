//! OpenAI Chat Completions 引擎（第二版 §2.3 openai.rs；同时服务 openai / ollama /
//! groq / deepseek / openrouter，靠 base_url + key 区分）。
//!
//! TODO(09)：填实现。四个已知怪癖（参考 goose
//! `crates/goose-provider-types/src/formats/openai.rs`）：
//!
//! 1. 带 tool_calls 的 assistant 消息 `content` 必须为 null 或 ""，不能省略字段；
//! 2. function name 只允许 `[A-Za-z0-9_-]{1,64}`（[`sanitize_function_name`]）；
//! 3. tool 消息必须紧跟含对应 tool_calls 的 assistant 消息；
//! 4. arguments 为空串时当 `{}`。
//!
//! 构造入参 = provider JSON 定义，供 `10` registry 调用。

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::Request;
use crate::provider::StreamEvent;

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    pub def: ProviderDef,
    /// 从 `def.api_key_env` 指定的环境变量读取。
    pub api_key: String,
}

impl OpenAiProvider {
    pub fn new(_def: &ProviderDef) -> crate::Result<Self> {
        todo!("TODO(09)")
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.def.name
    }

    /// `POST {base_url}/chat/completions`，`stream: true` +
    /// `stream_options: {include_usage: true}`，`Authorization: Bearer`。TODO(09)
    async fn stream(
        &self,
        _req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        todo!("TODO(09)")
    }
}

/// MCP 等来源的工具名先 sanitize 再上线（goose formats/openai.rs:1918）。TODO(09)
pub fn sanitize_function_name(_name: &str) -> String {
    todo!("TODO(09)")
}
