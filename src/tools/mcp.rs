//! `McpSource`：插件 `mcp.json` 的每个 server 一个实例（第三版 §2.5）。
//!
//! TODO(14)：填实现。rmcp 适配（内核里的"规范组件运行时"，约 300 行）：
//! stdio 经 `03` 的 `spawn_long_lived_mcp_subprocess`（进程组 + kill_on_drop，
//! stderr 接日志）；streamable-http 用 `StreamableHttpClientTransport::from_uri`；
//! `sse` 跳过并给"不支持"信息；`initialize` 的 `instructions` 存下供系统提示；
//! list_tools → `<server>__<tool>`（`annotations.readOnlyHint` → read_only）；
//! call_tool 拼 text 块、is_error 直映射、超时 300s；progress / logging 通知
//! 只写日志。运行时私有字段（rmcp session 等）由 `14` 在本结构补充。

use async_trait::async_trait;
use serde_json::Value;

use crate::plugin::mcp_config::McpServerConfig;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

/// 单次 call_tool 超时（第二版 §2.4）。
pub const CALL_TIMEOUT_SECS: u64 = 300;

#[derive(Debug)]
pub struct McpSource {
    /// `mcp:<plugin>/<server>`。
    pub id: String,
    pub server: McpServerConfig,
    /// `initialize` 返回的 instructions（系统提示注入位，`16`/`18` 接线）。
    pub instructions: Option<String>,
}

impl McpSource {
    /// 建立连接并 initialize。TODO(14)
    pub async fn connect(_plugin: &str, _server: &McpServerConfig) -> crate::Result<Self> {
        todo!("TODO(14)")
    }
}

#[async_trait]
impl ToolSource for McpSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        todo!("TODO(14)")
    }

    async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        todo!("TODO(14)")
    }

    /// server 被 kill 后：错误可读且不挂起（第二版 §5 P2b）。TODO(14)
    async fn shutdown(&self) {
        todo!("TODO(14)")
    }
}
