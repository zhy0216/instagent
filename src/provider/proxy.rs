//! `engine: "proxy"`（第三版 §2.4）：拉起插件自带的命令，得到本地 openai 兼容
//! 端点后当 openai 引擎用。
//!
//! TODO(11)：填实现。选空闲端口 → `args` 里替换 `${PORT}`、设环境变量
//! `INSTAGENT_PORT` → 轮询 `GET http://127.0.0.1:{port}{ready}` 到 200 →
//! 会话结束 kill（用 `03` 的进程组机制，Drop 里收口）→ 连接失败自动重启一次。
//! 环境变量透传白名单：`PATH` `HOME` `LANG` + 插件声明的 `env`。

use async_trait::async_trait;
use futures::stream::BoxStream;
use std::path::Path;

use crate::error::ProviderError;
use crate::provider::openai::OpenAiProvider;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::Request;
use crate::provider::StreamEvent;

#[derive(Debug)]
pub struct ProxyProvider {
    /// 指向 `http://127.0.0.1:{port}` 的 openai 引擎。
    pub inner: OpenAiProvider,
    pub port: u16,
}

impl ProxyProvider {
    /// 拉起 + 就绪轮询。子进程句柄等运行时状态由 `11` 在本结构补私有字段。
    pub async fn start(_def: &ProviderDef, _plugin_root: &Path) -> crate::Result<Self> {
        todo!("TODO(11)")
    }

    pub fn endpoint(&self) -> String {
        todo!("TODO(11)")
    }
}

#[async_trait]
impl Provider for ProxyProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.inner.stream(req).await
    }
}
