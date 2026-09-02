//! 插件模型（第三版 §2）。
//!
//! 内核 = 加载器（manifest 校验 / 发现 / 启用 / 安装）+ 规范组件运行时
//! （skills 加载器、MCP client）。可移植组件只有 skills 与 mcp.json，
//! 其余（providers / hooks / commands / command tools）放反域名命名空间
//! [`NAMESPACE`] 目录下。

pub mod bundled;
pub mod discovery;
pub mod install;
pub mod manifest;
pub mod mcp_config;

pub use discovery::PluginSet;
pub use manifest::PluginManifest;

use std::path::PathBuf;

/// 本仓库的反域名命名空间（计划里的 `dev.miniagent`，见 AGENTS.md 命名对照）。
pub const NAMESPACE: &str = "dev.instagent";

/// 插件发现来源。同名覆盖按优先级：Extra/CLI 参数 > 项目层 > 用户层 > Bundled
/// （用户插件覆盖 bundled，第三版 §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Bundled,
    User,
    Project,
    /// 配置 `plugins` 额外路径与 `--plugin PATH`。
    Extra,
}

/// 一个已通过 manifest 校验、已启用的插件。
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    /// 插件根目录绝对路径；`${PLUGIN_ROOT}` 的展开值。
    pub root: PathBuf,
    pub source: PluginSource,
}
