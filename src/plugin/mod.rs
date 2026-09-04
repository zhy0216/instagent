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

use std::path::Path;
use std::path::PathBuf;

use crate::settings::Settings;

/// 本仓库的反域名命名空间（计划里的 `dev.miniagent`，见 AGENTS.md 命名对照）。
pub const NAMESPACE: &str = "dev.instagent";

/// 插件发现来源。同名覆盖按优先级：CLI 参数 > 配置 `Extra` 路径 > 项目层 >
/// 用户层 > Bundled（用户插件覆盖 bundled，第三版 §1）。CLI 与 `Extra` 同层，
/// 但是独立 kind（E8）：诊断能区分"运行时 `--plugin` 显式路径"与配置文件路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Bundled,
    User,
    Project,
    /// 配置 `plugins` 额外路径。
    Extra,
    /// 运行时 `--plugin PATH`。
    Cli,
}

impl PluginSource {
    /// 诊断显示名：与解析后的绝对路径一起出现在 skipped / 错误文案里。
    pub fn display_name(self) -> &'static str {
        match self {
            PluginSource::Bundled => "bundled",
            PluginSource::User => "user plugin dir",
            PluginSource::Project => "project plugin dir",
            PluginSource::Extra => "configured plugin path",
            PluginSource::Cli => "CLI --plugin path",
        }
    }
}

/// 诊断用路径：能解析就用绝对路径（缺失文件也走 `std::path::absolute`，
/// 仍失败才退回原样），让 skipped 同时说清"哪个来源、哪条路径"。
pub(crate) fn located(path: &Path) -> String {
    let abs = std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());
    abs.display().to_string()
}

/// 单个插件名的启用判定（第三版 §2.10 + ADR 0003 D5）。白名单（含显式 `[]`
/// = 禁用全部）说了算；从未表态才看 `disabledPlugins`。discovery 与 bundled
/// 共用，避免两处各自 `is_empty()` 把终值 `[]` 读成"全启用"。
pub fn plugin_enabled(name: &str, settings: &Settings) -> bool {
    match settings.whitelist() {
        Some(names) => names.iter().any(|enabled| enabled == name),
        None => !settings.disabled_plugins.iter().any(|n| n == name),
    }
}

/// 一个已通过 manifest 校验、已启用的插件。
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    /// 插件根目录绝对路径；`${PLUGIN_ROOT}` 的展开值。
    pub root: PathBuf,
    pub source: PluginSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0003 D5 的三态落到启用判定（discovery 与 bundled 共用同一规则）。
    #[test]
    fn plugin_enabled_follows_the_three_states() {
        // 缺失 = 不表态 → 黑名单模式。
        assert!(plugin_enabled("a", &Settings::default()));
        let blacklist = Settings {
            disabled_plugins: vec!["a".into()],
            ..Settings::default()
        };
        assert!(!plugin_enabled("a", &blacklist));
        assert!(plugin_enabled("b", &blacklist));

        // 非空 = 白名单：没列出的不启用。
        let whitelist = Settings {
            enabled_plugins: vec!["a".into()],
            ..Settings::default()
        };
        assert!(plugin_enabled("a", &whitelist));
        assert!(!plugin_enabled("b", &whitelist));

        // 显式 [] = 终值禁用全部（03 残留：不再退化成"全启用"）。
        let locked = Settings {
            enabled_locked: true,
            ..Settings::default()
        };
        assert!(!plugin_enabled("a", &locked));
        assert!(!plugin_enabled("bundled", &locked));
    }

    /// 每个来源都有独立显示名（E8：诊断能区分 CLI 与其它来源）。
    #[test]
    fn sources_have_distinct_display_names() {
        let names: Vec<&str> = [
            PluginSource::Bundled,
            PluginSource::User,
            PluginSource::Project,
            PluginSource::Extra,
            PluginSource::Cli,
        ]
        .iter()
        .map(|source| source.display_name())
        .collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        assert!(names.iter().all(|name| !name.is_empty()));
    }
}
