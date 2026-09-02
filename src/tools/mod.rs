//! 工具层：`ToolSource` trait + Registry + 命名规则（第三版 §2.5；第二版 §2.4）。
//!
//! 四个来源实现：[`BuiltinTools`]（内核 5 工具）、[`McpSource`]（插件 mcp.json
//! 的每个 server 一个实例）、[`CommandTools`]（`dev.instagent/tools/*.json`）、
//! [`SkillsSource`]（只暴露 load_skill）。
//!
//! TODO(13)：填 Registry 的路由与命名（内置不加前缀；MCP `<server>__<tool>`；
//! command tools `<plugin>__<tool>`；冲突再加插件名；超 64 字符截断 + 6 位哈希，
//! 双向映射表同一会话内稳定）。

pub mod builtin;
pub mod command;
pub mod mcp;
pub mod skills;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use builtin::BuiltinTools;
pub use command::CommandTools;
pub use mcp::McpSource;
pub use skills::SkillsSource;

/// MCP / command 工具名分隔符（与 goose 相同）。
pub const NAME_SEP: &str = "__";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// schema 手写 JSON，不用 schemars（第二版 §2.4）。
    pub input_schema: Value,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(_text: String) -> Self {
        todo!("TODO(13)")
    }

    pub fn err(_text: String) -> Self {
        todo!("TODO(13)")
    }

    /// 审批拒绝：以 is_error 结果回给模型（"user denied: <reason>"）。TODO(13)
    pub fn denied(_reason: &str) -> Self {
        todo!("TODO(13)")
    }
}

/// 一次待执行的工具调用（由 `Content::ToolUse` 而来）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// 工具执行环境：会话目录 + 取消令牌。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
}

/// 工具来源（第三版 §2.5）。替换的是实现，不是动态加载。
#[async_trait]
pub trait ToolSource: Send + Sync {
    /// `"builtin"` | `"mcp:<plugin>/<server>"` | `"cmd:<plugin>"` | `"skills"`
    fn id(&self) -> &str;

    async fn list(&self) -> Vec<ToolSpec>;

    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput;

    async fn shutdown(&self) {}
}

/// 汇总多个来源，按模型可见名路由（名字映射表在实现里，双向且会话内稳定）。
#[derive(Default)]
pub struct Registry {
    pub sources: Vec<Arc<dyn ToolSource>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, _source: Arc<dyn ToolSource>) {
        todo!("TODO(13)")
    }

    /// 汇总各来源 spec，套命名规则 + 64 字符映射。TODO(13)
    pub async fn list(&self) -> Vec<ToolSpec> {
        todo!("TODO(13)")
    }

    /// 按映射表路由回真实 (source, name)。TODO(13)
    pub async fn call(&self, _call: &ToolCall, _ctx: &ToolCtx) -> ToolOutput {
        todo!("TODO(13)")
    }

    pub async fn shutdown(&self) {
        todo!("TODO(13)")
    }
}

/// OpenAI 函数名只允许 `[A-Za-z0-9_-]{1,64}`：非法字符替换、超长截断后
/// 加 6 位哈希（映射表由 Registry 维护）。TODO(13)
pub fn model_visible_name(_name: &str) -> String {
    todo!("TODO(13)")
}
