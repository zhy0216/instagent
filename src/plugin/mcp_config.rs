//! 插件 `mcp.json`（规范格式）解析 + 变量展开 + `.mcp.json` 兼容（第三版 §2.3）。
//!
//! TODO(06)：填实现。只展开 `${PLUGIN_ROOT}` / `${PLUGIN_DATA}`（单次、非递归，
//! 只作用于 `args` 元素 / `env` 值 / `cwd`）；`env` 里不得定义这两个名字；
//! `sse` 类型标记"不支持"由上层跳过；无 `mcp.json` 时回退读 `.mcp.json`
//! （goose / Claude Code 草案，无 `type` 字段按 stdio）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::plugin::Plugin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerType {
    Stdio,
    StreamableHttp,
    /// v1 不实现，跳过时给可读提示。
    Sse,
}

/// 单个 MCP server 的运行时配置（已完成变量展开）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 插件内的 server 名（工具前缀 `<server>__<tool>` 用）。
    pub name: String,
    pub r#type: McpServerType,
    /// stdio：单个可执行名或 `./` 开头的插件相对路径，不做变量展开。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// streamable-http。`headers` 不承载凭据（规范规定，远程鉴权 v1 不做）。
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// 读插件的 `mcp.json`（或草案 `.mcp.json`）并展开变量。TODO(06)
pub fn load_servers(_plugin: &Plugin) -> crate::Result<Vec<McpServerConfig>> {
    todo!("TODO(06)")
}
