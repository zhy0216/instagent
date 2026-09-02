//! 配置（第二版 §2.10；第三版 §2.10：无 `mcp:` 段，多 `plugins` 额外路径）。
//!
//! TODO(01)：填 `Config::default` / `load` / `save` 与 `INSTAGENT_{PROVIDER,MODEL,MODE}`
//! 环境变量覆盖；类型布局已由 00 锁定。

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// 审批模式（第二版 §2.8）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 全放行。
    Auto,
    /// 白名单放行，其余问用户（默认）。
    #[default]
    Approve,
    /// 不给模型工具。
    Chat,
}

/// `~/.config/instagent/config.yaml` 的完整形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// provider 名字（定义来自插件，第三版 §2.4）。
    pub provider: Option<String>,
    pub model: Option<String>,
    /// 优先读该环境变量取密钥；也允许 `api_key` 直接写（文件 0600）。
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub max_tokens: u32,
    pub mode: Mode,
    /// 默认 1000（goose agent.rs:85）。
    pub max_turns: u32,
    /// 覆盖模型前缀小表（第二版 §2.3）。
    pub context_limit: Option<u32>,
    /// 压缩触发阈值，默认 0.8。
    pub compaction_threshold: f32,
    /// 默认 $SHELL。
    pub shell: Option<String>,
    /// 审批白名单；`16` 的 AllowAlways 即时写回。
    pub always_allow: Vec<String>,
    /// 额外插件搜索路径。
    pub plugins: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        todo!("TODO(01)")
    }
}

impl Config {
    /// 读用户级 config.yaml，叠加环境变量覆盖。TODO(01)
    pub fn load(_cwd: &Path) -> crate::Result<Config> {
        todo!("TODO(01)")
    }

    /// 写回用户级 config.yaml（`always_allow` 持久化用，第二版 §2.8）。TODO(01)
    pub fn save(&self) -> crate::Result<()> {
        todo!("TODO(01)")
    }
}
