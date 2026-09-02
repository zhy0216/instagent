//! 插件发现与启用（第三版 §2.10）。
//!
//! TODO(05)：填 `discover`（扫描顺序、同名覆盖、manifest 失败记录并跳过）与
//! enabled/disabled/trusted 判定（用 `01` 的三层 settings，白名单模式）。
//! 参考 goose `plugins/discovery.rs`（只读）。

use std::path::Path;
use std::path::PathBuf;

use crate::plugin::Plugin;
use crate::settings::Settings;

/// 启用的插件集合；迭代得到 {manifest, root, source}，供 `06/07/10/13/15` 使用。
#[derive(Debug, Clone, Default)]
pub struct PluginSet {
    pub plugins: Vec<Plugin>,
}

impl PluginSet {
    pub fn iter(&self) -> std::slice::Iter<'_, Plugin> {
        self.plugins.iter()
    }

    /// 按裸名查找；重名（`plugin/name` 消歧）由 `10` 在 provider 层处理。
    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.manifest.name == name)
    }
}

/// 扫描 `~/.agents/plugins/`、`<project>/.agents/plugins/`、配置 `plugins`
/// 额外路径与 `--plugin PATH`；应用三层 settings 的启用判定。TODO(05)
pub fn discover(
    _cwd: &Path,
    _settings: &Settings,
    _extra_paths: &[PathBuf],
    _cli_plugins: &[PathBuf],
) -> crate::Result<PluginSet> {
    todo!("TODO(05)")
}
