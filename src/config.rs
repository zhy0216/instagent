//! 配置（第二版 §2.10；第三版 §2.10：无 `mcp:` 段，多 `plugins` 额外路径）。
//!
//! 读 `~/.config/instagent/config.yaml`，叠加 `INSTAGENT_{PROVIDER,MODEL,MODE}`
//! 环境变量覆盖；不接系统 keyring。用户级目录可被 `INSTAGENT_CONFIG_DIR`
//! 覆盖（测试用），与 `session.rs` 的 `INSTAGENT_DATA_DIR` 同一约定。

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
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

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => Mode::Auto,
            "approve" => Mode::Approve,
            "chat" => Mode::Chat,
            other => anyhow::bail!("invalid mode `{other}` (expected auto|approve|chat)"),
        })
    }
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
        Self {
            provider: None,
            model: None,
            api_key_env: None,
            api_key: None,
            max_tokens: 8192,
            mode: Mode::default(),
            max_turns: 1000,
            context_limit: None,
            compaction_threshold: 0.8,
            shell: None,
            always_allow: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

/// 用户级配置目录：`INSTAGENT_CONFIG_DIR` 覆盖，否则 `<config_base>/instagent`
/// （`~/.config/instagent`，第二版 §2.10 命名对照）。
pub fn config_dir() -> crate::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("INSTAGENT_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    use etcetera::base_strategy::BaseStrategy as _;
    let strategy = etcetera::base_strategy::choose_base_strategy()?;
    Ok(strategy.config_dir().join("instagent"))
}

impl Config {
    /// 读用户级 config.yaml，叠加环境变量覆盖，并把 `plugins` 路径展开
    /// （`~` 前缀 + 相对路径按 `cwd` 解析）。
    pub fn load(cwd: &Path) -> crate::Result<Config> {
        let path = config_dir()?.join("config.yaml");
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_yaml::from_str::<Config>(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        apply_env_overrides(&mut config)?;
        for plugin in &mut config.plugins {
            let raw = plugin.display().to_string();
            let expanded = shellexpand::tilde(&raw);
            let path = PathBuf::from(expanded.as_ref());
            *plugin = if path.is_relative() {
                cwd.join(path)
            } else {
                path
            };
        }
        Ok(config)
    }

    /// 写回用户级 config.yaml（`always_allow` 持久化用，第二版 §2.8），
    /// 文件权限 0600（可能含 `api_key`）。
    pub fn save(&self) -> crate::Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("config.yaml");
        std::fs::write(&path, serde_yaml::to_string(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

fn env_override(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn apply_env_overrides(config: &mut Config) -> crate::Result<()> {
    if let Some(value) = env_override("INSTAGENT_PROVIDER") {
        config.provider = Some(value);
    }
    if let Some(value) = env_override("INSTAGENT_MODEL") {
        config.model = Some(value);
    }
    if let Some(value) = env_override("INSTAGENT_MODE") {
        config.mode = value
            .parse()
            .with_context(|| format!("invalid INSTAGENT_MODE: {value}"))?;
    }
    Ok(())
}

/// `INSTAGENT_*` 是进程级环境变量，`config` 与 `settings` 的单测共用这把
/// 锁串行访问，避免并行干扰。
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_dir() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let guard = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("INSTAGENT_CONFIG_DIR", dir.path());
        for key in ["INSTAGENT_PROVIDER", "INSTAGENT_MODEL", "INSTAGENT_MODE"] {
            std::env::remove_var(key);
        }
        (guard, dir)
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let (_guard, _dir) = temp_config_dir();
        let cwd = Path::new("/tmp/project");
        let config = Config::load(cwd).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.mode, Mode::Approve);
        assert_eq!(config.max_turns, 1000);
        assert!((config.compaction_threshold - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn save_load_round_trip() {
        let (_guard, dir) = temp_config_dir();
        let config = Config {
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-5".into()),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            max_tokens: 4096,
            mode: Mode::Chat,
            max_turns: 50,
            context_limit: Some(200_000),
            compaction_threshold: 0.65,
            shell: Some("/bin/zsh".into()),
            always_allow: vec!["read".into(), "everything__echo".into()],
            plugins: vec![dir.path().join("extra-plugins")],
            ..Config::default()
        };
        config.save().unwrap();
        let loaded = Config::load(Path::new("/tmp/project")).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn partial_yaml_uses_defaults_for_absent_fields() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "provider: openai\nmodel: gpt-5\n",
        )
        .unwrap();
        let config = Config::load(Path::new("/tmp/project")).unwrap();
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(config.model.as_deref(), Some("gpt-5"));
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.mode, Mode::Approve);
        assert!(config.always_allow.is_empty());
    }

    #[test]
    fn env_vars_override_file() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "provider: openai\nmodel: gpt-5\nmode: chat\n",
        )
        .unwrap();
        std::env::set_var("INSTAGENT_PROVIDER", "proxy");
        std::env::set_var("INSTAGENT_MODEL", "overridden-model");
        std::env::set_var("INSTAGENT_MODE", "auto");
        let config = Config::load(Path::new("/tmp/project")).unwrap();
        assert_eq!(config.provider.as_deref(), Some("proxy"));
        assert_eq!(config.model.as_deref(), Some("overridden-model"));
        assert_eq!(config.mode, Mode::Auto);
    }

    #[test]
    fn invalid_mode_env_is_error() {
        let (_guard, _dir) = temp_config_dir();
        std::env::set_var("INSTAGENT_MODE", "yolo");
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        assert!(err.to_string().contains("INSTAGENT_MODE"));
    }

    #[test]
    fn plugin_paths_resolved_against_cwd() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "plugins:\n  - extra/plugins\n  - /abs/plugins\n",
        )
        .unwrap();
        let cwd = Path::new("/work/demo");
        let config = Config::load(cwd).unwrap();
        assert_eq!(
            config.plugins,
            vec![cwd.join("extra/plugins"), PathBuf::from("/abs/plugins")]
        );
    }
}
