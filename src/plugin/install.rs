//! 插件安装 / 更新（第三版 §2.10；逻辑参考 goose `plugins/mod.rs`，只读）。
//!
//! TODO(07)：填实现。git-url 用 `03` 的 `git_command` clone（不引 libgit2），
//! 本地路径复制 → `04` 校验 → 写 `.install.json` → 放入 `~/.agents/plugins/`。
//! list / enable / disable / show 的数据层也归本文件，CLI 接线在 `18`。

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::plugin::Plugin;

/// 安装来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    GitUrl(String),
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    pub auto_update: bool,
}

/// `.install.json` 的内容（goose `.goose-plugin-install.json` 的对应物）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub source: String,
    pub commit: Option<String>,
    /// epoch 秒。
    pub installed_at: i64,
    /// 24h 自动更新节流（goose 同款）用。
    pub last_update_check: Option<i64>,
    pub auto_update: bool,
}

/// `PLUGIN_DATA`：`<data_dir>/plugins/<name>/`，按需创建。TODO(07)
pub fn plugin_data_dir(_name: &str) -> crate::Result<PathBuf> {
    todo!("TODO(07)")
}

pub fn install(_source: &InstallSource, _opts: &InstallOptions) -> crate::Result<Plugin> {
    todo!("TODO(07)")
}

/// 按 `.install.json` 重新拉取。TODO(07)
pub fn update(_name: &str) -> crate::Result<()> {
    todo!("TODO(07)")
}
