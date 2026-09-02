//! 三层 settings（第三版 §2.10，goose 草案字段名）。
//!
//! TODO(01)：文件读取与合并（local > project > user）；启用判定逻辑在 TODO(05)。

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

/// settings 文件来源层，优先级 Local > Project > User。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsLayer {
    /// `~/.config/instagent/settings.json`
    User,
    /// `<project>/.config/instagent/settings.json`
    Project,
    /// 同目录 `settings.local.json`
    Local,
}

/// 三层合并后的 settings 形状（字段名按规范为 camelCase）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 写了即白名单模式；没写则"除 `disabled_plugins` 外全启用"。
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
    /// 首次启用确认的结果（第三版 §2.10 信任），CLI 接线在 `18`。
    pub trusted_plugins: Vec<String>,
}

impl Settings {
    /// 读三层文件并合并（local > project > user）。TODO(01)
    pub fn merged(_cwd: &Path) -> crate::Result<Settings> {
        todo!("TODO(01)")
    }

    /// 写回指定层（`18` 的 enable/disable 与信任确认用）。TODO(01)
    pub fn save(&self, _cwd: &Path, _layer: SettingsLayer) -> crate::Result<()> {
        todo!("TODO(01)")
    }
}
