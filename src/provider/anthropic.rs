//! 原生 Anthropic Messages API 引擎（第二版 §2.3 anthropic.rs），
//! 整个模块挂在 cargo feature `anthropic-engine` 后（`12` 填实现）。
//!
//! TODO(12)：`POST {base}/v1/messages`，`stream: true`，头 `x-api-key` +
//! `anthropic-version: 2023-06-01`；SSE 事件链 message_start →
//! content_block_start → content_block_delta(text_delta | input_json_delta) →
//! content_block_stop → message_delta → message_stop；`input_json_delta` 全部
//! 拼完再 parse；`cache_control: {type: ephemeral}` 加在 system 块与最后一个
//! tool spec（参考 goose `formats/anthropic.rs:230/489/871`，只读）。

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::Request;
use crate::provider::StreamEvent;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub def: ProviderDef,
    pub api_key: String,
}

impl AnthropicProvider {
    pub fn new(_def: &ProviderDef) -> crate::Result<Self> {
        todo!("TODO(12)")
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.def.name
    }

    async fn stream(
        &self,
        _req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        todo!("TODO(12)")
    }
}
