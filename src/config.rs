//! 配置（第二版 §2.10；第三版 §2.10：无 `mcp:` 段，多 `plugins` 额外路径）。
//!
//! 读 `~/.config/instagent/config.yaml`，叠加 `INSTAGENT_{PROVIDER,MODEL}`
//! 环境变量覆盖；不接系统 keyring。用户级目录可被 `INSTAGENT_CONFIG_DIR`
//! 覆盖（测试用），与 `session.rs` 的 `INSTAGENT_DATA_DIR` 同一约定。

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

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
    /// 默认 1000（goose agent.rs:85）。
    pub max_turns: u32,
    /// 覆盖模型前缀小表（第二版 §2.3）。
    pub context_limit: Option<u32>,
    /// 压缩触发阈值，默认 0.8。
    pub compaction_threshold: f32,
    /// 默认 $SHELL。
    pub shell: Option<String>,
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
            max_turns: 1000,
            context_limit: None,
            compaction_threshold: 0.8,
            shell: None,
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
        for key in ["INSTAGENT_PROVIDER", "INSTAGENT_MODEL"] {
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
        assert_eq!(config.max_turns, 1000);
        assert!((config.compaction_threshold - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn load_reads_full_yaml() {
        let (_guard, dir) = temp_config_dir();
        let config = Config {
            provider: Some("openai".into()),
            model: Some("gpt-4o".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            api_key: None,
            max_tokens: 4096,
            max_turns: 50,
            context_limit: Some(200_000),
            compaction_threshold: 0.65,
            shell: Some("/bin/zsh".into()),
            plugins: vec![dir.path().join("extra-plugins")],
        };
        std::fs::write(
            dir.path().join("config.yaml"),
            format!(
                "provider: openai\nmodel: gpt-4o\napi_key_env: OPENAI_API_KEY\n\
                 max_tokens: 4096\nmax_turns: 50\ncontext_limit: 200000\n\
                 compaction_threshold: 0.65\nshell: /bin/zsh\nplugins:\n  - {}\n",
                dir.path().join("extra-plugins").display()
            ),
        )
        .unwrap();
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
    }

    #[test]
    fn env_vars_override_file() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "provider: openai\nmodel: gpt-5\n",
        )
        .unwrap();
        std::env::set_var("INSTAGENT_PROVIDER", "proxy");
        std::env::set_var("INSTAGENT_MODEL", "overridden-model");
        let config = Config::load(Path::new("/tmp/project")).unwrap();
        assert_eq!(config.provider.as_deref(), Some("proxy"));
        assert_eq!(config.model.as_deref(), Some("overridden-model"));
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
