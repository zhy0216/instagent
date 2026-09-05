//! 配置（第二版 §2.10；第三版 §2.10：无 `mcp:` 段，多 `plugins` 额外路径）。
//!
//! 读 `~/.config/instagent/config.yaml`，叠加 `INSTAGENT_{PROVIDER,MODEL}`
//! 环境变量覆盖；不接系统 keyring。用户级目录可被 `INSTAGENT_CONFIG_DIR`
//! 覆盖（测试用），与 `session.rs` 的 `INSTAGENT_DATA_DIR` 同一约定。
//!
//! 密钥语义由 ADR 0003 D1 钉死（todo 08）：config.yaml 不接受任何形式的
//! 密钥——`api_key` / `api_key_env` 键在加载期报错并给迁移提示；密钥唯一
//! 来源是 provider JSON `api_key_env` 指向的环境变量。原始密钥不持久化、
//! 不进日志；加载错误不回显字段值。
//!
//! 字段级校验（todo 08 / S7）在装载边界完成：非法数值 / 空字符串 / 空路径
//! 都在 `load` 失败，错误带来源文件、字段与建议值。

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

/// 单个 config.yaml 的读取字节预算（方案默认：config/settings 1 MiB）。
/// 超限直接报错并保留原文件，不做截断读取、不回显原文（todo 08 / D03）。
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// `~/.config/instagent/config.yaml` 的完整形状。
///
/// 不再包含任何密钥字段（ADR 0003 D1）：旧 `api_key` / `api_key_env` 键
/// 由 [`Config::load`] 在加载期显式拒绝。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    /// provider 名字（定义来自插件，第三版 §2.4）。
    pub provider: Option<String>,
    pub model: Option<String>,
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
            max_tokens: 8192,
            max_turns: 1000,
            context_limit: None,
            compaction_threshold: 0.8,
            shell: None,
            plugins: Vec::new(),
        }
    }
}

/// `Config` 的反序列化影子（todo 08 / S7）：数值先按宽容类型（i64 / f64）
/// 读入，再经 [`RawConfig::into_config`] 逐项校验转换，让负数、超大值、
/// NaN、越界阈值都得到「来源文件 + 字段 + 建议值」的诊断，而不是 serde
/// 类型错误。字段缺省即 [`Config::default`] 的对应值。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    provider: Option<String>,
    model: Option<String>,
    max_tokens: Option<i64>,
    max_turns: Option<i64>,
    context_limit: Option<i64>,
    compaction_threshold: Option<f64>,
    shell: Option<String>,
    plugins: Vec<PathBuf>,
}

impl RawConfig {
    fn into_config(self, source: &str) -> crate::Result<Config> {
        Ok(Config {
            provider: self.provider,
            model: self.model,
            max_tokens: required_positive_u32(source, "max_tokens", self.max_tokens, 8192)?,
            max_turns: required_positive_u32(source, "max_turns", self.max_turns, 1000)?,
            context_limit: optional_positive_u32(source, "context_limit", self.context_limit)?,
            compaction_threshold: compaction_threshold(source, self.compaction_threshold)?,
            shell: self.shell,
            plugins: {
                for (index, path) in self.plugins.iter().enumerate() {
                    if path.as_os_str().is_empty() {
                        bail!(
                            "{source}: field `plugins[{index}]` must be a non-empty path \
                             (suggested: remove the empty entry)"
                        );
                    }
                }
                self.plugins
            },
        })
    }
}

fn required_positive_u32(
    source: &str,
    field: &str,
    value: Option<i64>,
    default: u32,
) -> crate::Result<u32> {
    match value {
        None => Ok(default),
        Some(raw) => match u32::try_from(raw).ok().filter(|value| *value >= 1) {
            Some(value) => Ok(value),
            None => bail!(
                "{source}: field `{field}` must be an integer between 1 and {} \
                 (got {raw}; suggested: {default})",
                u32::MAX
            ),
        },
    }
}

fn optional_positive_u32(
    source: &str,
    field: &str,
    value: Option<i64>,
) -> crate::Result<Option<u32>> {
    match value {
        None => Ok(None),
        Some(raw) => match u32::try_from(raw).ok().filter(|value| *value >= 1) {
            Some(value) => Ok(Some(value)),
            None => bail!(
                "{source}: field `{field}` must be an integer between 1 and {} \
                 (got {raw}; suggested: remove the field to let the provider/model \
                 table decide)",
                u32::MAX
            ),
        },
    }
}

fn compaction_threshold(source: &str, value: Option<f64>) -> crate::Result<f32> {
    match value {
        None => Ok(0.8),
        Some(raw) => {
            if !raw.is_finite() || raw <= 0.0 || raw > 1.0 {
                bail!(
                    "{source}: field `compaction_threshold` must be a finite number in (0, 1] \
                     (got {raw}; suggested: 0.8)"
                );
            }
            let narrowed = raw as f32;
            // f64→f32 可能下溢到 0（极小正数）：转后仍须正且有限，否则阈值
            // 退化成"永不/永远压缩"（todo 08 / D03）。
            if !narrowed.is_finite() || narrowed <= 0.0 || narrowed > 1.0 {
                bail!(
                    "{source}: field `compaction_threshold` must be a finite number in (0, 1] \
                     after f32 narrowing (got {raw}; suggested: 0.8)"
                );
            }
            Ok(narrowed)
        }
    }
}

fn non_empty_optional(source: &str, field: &str, value: Option<String>) -> crate::Result<()> {
    match &value {
        Some(text) if text.trim().is_empty() => bail!(
            "{source}: field `{field}` must be a non-empty string \
             (suggested: remove the field or set a real value)"
        ),
        _ => Ok(()),
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
    /// 读用户级 config.yaml，做字段级校验（错误带来源文件、字段与建议值），
    /// 叠加环境变量覆盖，并把 `plugins` 路径展开（`~` 前缀 + 相对路径按
    /// `cwd` 解析）。含 `api_key` / `api_key_env` 的文件在加载期报错
    /// （ADR 0003 D1）。单文件有界读取（[`MAX_CONFIG_FILE_BYTES`]）：超限 /
    /// 非 UTF-8 / 坏 YAML 都带来源报错、不回显原文，原文件保持不变。
    pub fn load(cwd: &Path) -> crate::Result<Config> {
        let path = config_dir()?.join("config.yaml");
        let source = path.display().to_string();
        let mut config = match read_config_file(&path, &source)? {
            Some(text) => {
                reject_forbidden_key_fields(&text, &source)?;
                let raw: RawConfig =
                    serde_yaml::from_str(&text).with_context(|| format!("in {source}"))?;
                raw.into_config(&source)?
            }
            None => Config::default(),
        };
        let (provider_from_env, model_from_env) = apply_env_overrides(&mut config);
        non_empty_optional(
            override_source("INSTAGENT_PROVIDER", provider_from_env, &source),
            "provider",
            config.provider.clone(),
        )?;
        non_empty_optional(
            override_source("INSTAGENT_MODEL", model_from_env, &source),
            "model",
            config.model.clone(),
        )?;
        non_empty_optional(&source, "shell", config.shell.clone())?;
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
        config.validate_merged(&source)?;
        Ok(config)
    }

    /// 合并后校验（todo 08 / D03）：文件 + 环境 + CLI override 全部合并后、
    /// provider/MCP 启动前调用。拒绝空白 provider/model/shell 与非正/非有限
    /// 的 f32 阈值（含 f64→f32 下溢到 0 的极小值）；错误带来源，不回显原文。
    /// [`Config::load`] 已在返回前调用一次；CLI `-m` 等覆盖后由 assembly 再
    /// 调一次，来源指向 CLI 合并结果。
    pub fn validate_merged(&self, source: &str) -> crate::Result<()> {
        non_empty_optional(source, "provider", self.provider.clone())?;
        non_empty_optional(source, "model", self.model.clone())?;
        non_empty_optional(source, "shell", self.shell.clone())?;
        if !self.compaction_threshold.is_finite()
            || self.compaction_threshold <= 0.0
            || self.compaction_threshold > 1.0
        {
            bail!(
                "{source}: field `compaction_threshold` must be a finite number in (0, 1] \
                 (got {}; suggested: 0.8)",
                self.compaction_threshold
            );
        }
        for (index, path) in self.plugins.iter().enumerate() {
            if path.as_os_str().is_empty() {
                bail!(
                    "{source}: field `plugins[{index}]` must be a non-empty path \
                     (suggested: remove the empty entry)"
                );
            }
        }
        Ok(())
    }
}

/// 有界读取 config.yaml：`NotFound` 按无配置处理（`None`）；超限/非 UTF-8/
/// IO 错误都带来源报错、不回显原文，原文件保持不变（只读，从不截断改写）。
fn read_config_file(path: &Path, source: &str) -> crate::Result<Option<String>> {
    use std::io::Read as _;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read config file {source}")),
    };
    let mut buf = Vec::new();
    std::io::Read::take(&mut file, MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read config file {source}"))?;
    if buf.len() as u64 > MAX_CONFIG_FILE_BYTES {
        bail!(
            "{source}: config file exceeds the {MAX_CONFIG_FILE_BYTES}-byte read budget; \
             the file was left unchanged, trim it and retry"
        );
    }
    String::from_utf8(buf)
        .map_err(|_| {
            anyhow::anyhow!("{source}: config file is not valid UTF-8; the file was left unchanged")
        })
        .map(Some)
}

fn env_override(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// 覆盖字段的错误来源：被环境变量覆盖时指向变量名，否则指向配置文件路径。
fn override_source<'a>(var: &str, from_env: bool, file_source: &'a str) -> &'a str {
    if from_env {
        match var {
            "INSTAGENT_PROVIDER" => "env var INSTAGENT_PROVIDER",
            _ => "env var INSTAGENT_MODEL",
        }
    } else {
        file_source
    }
}

fn apply_env_overrides(config: &mut Config) -> (bool, bool) {
    let mut provider_from_env = false;
    let mut model_from_env = false;
    if let Some(value) = env_override("INSTAGENT_PROVIDER") {
        config.provider = Some(value);
        provider_from_env = true;
    }
    if let Some(value) = env_override("INSTAGENT_MODEL") {
        config.model = Some(value);
        model_from_env = true;
    }
    (provider_from_env, model_from_env)
}

/// ADR 0003 D1：config.yaml 不允许出现 `api_key` / `api_key_env`。加载期
/// 显式报错并给迁移提示；错误文案只提键名、绝不回显键值（密钥不进日志）。
fn reject_forbidden_key_fields(text: &str, source: &str) -> crate::Result<()> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).with_context(|| format!("in {source}"))?;
    let serde_yaml::Value::Mapping(map) = &value else {
        return Ok(());
    };
    for key in ["api_key", "api_key_env"] {
        if map.contains_key(serde_yaml::Value::String(key.to_string())) {
            bail!(
                "{source}: forbidden key `{key}`: config no longer accepts API keys \
                 (ADR 0003 D1); remove this field and declare the environment variable \
                 name in the provider JSON's `api_key_env` instead"
            );
        }
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
                "provider: openai\nmodel: gpt-4o\n\
                 max_tokens: 4096\nmax_turns: 50\ncontext_limit: 200000\n\
                 compaction_threshold: 0.65\nshell: /bin/zsh\nplugins:\n  - {}\n",
                dir.path().join("extra-plugins").display()
            ),
        )
        .unwrap();
        let loaded = Config::load(Path::new("/tmp/project")).unwrap();
        assert_eq!(loaded, config);
    }

    // ---- ADR 0003 D1：密钥唯一来源是 provider manifest，config 不接受密钥 ----

    #[test]
    fn legacy_api_key_field_rejected_with_migration_hint_and_no_leak() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "provider: openai\nmodel: gpt-5\napi_key: sk-supersecretvalue123\n",
        )
        .unwrap();
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        // 指出来源文件与字段。
        assert!(message.contains("config.yaml"), "{message}");
        assert!(message.contains("api_key"), "{message}");
        // 迁移提示指向 provider JSON 的 api_key_env。
        assert!(message.contains("api_key_env"), "{message}");
        assert!(message.contains("remove this field"), "{message}");
        // 原始密钥不进错误输出。
        assert!(!message.contains("sk-supersecretvalue123"), "{message}");
    }

    #[test]
    fn legacy_api_key_env_field_rejected_even_with_non_secret_value() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(
            dir.path().join("config.yaml"),
            "api_key_env: OPENAI_API_KEY\n",
        )
        .unwrap();
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("`api_key_env`"), "{message}");
        assert!(message.contains("ADR 0003 D1"), "{message}");
    }

    #[test]
    fn serialized_config_has_no_key_fields() {
        // 原始密钥不持久化：Config 根本没有密钥字段可写出去。
        let yaml = serde_yaml::to_string(&Config::default()).unwrap();
        assert!(!yaml.contains("api_key"), "{yaml}");
    }

    // ---- 字段级校验：每个非法值在装载边界失败，错误带来源/字段/建议值 ----

    #[test]
    fn zero_or_invalid_numerics_rejected_with_field_and_suggestion() {
        let (_guard, dir) = temp_config_dir();
        let cases = [
            ("max_tokens: 0\n", "max_tokens", "8192"),
            ("max_tokens: -1\n", "max_tokens", "8192"),
            ("max_tokens: 99999999999999\n", "max_tokens", "8192"),
            ("max_turns: 0\n", "max_turns", "1000"),
            ("context_limit: 0\n", "context_limit", "remove the field"),
        ];
        for (yaml, field, suggestion) in cases {
            std::fs::write(dir.path().join("config.yaml"), yaml).unwrap();
            let err = Config::load(Path::new("/tmp/project")).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("config.yaml"), "{yaml} → {message}");
            assert!(message.contains(field), "{yaml} → {message}");
            assert!(message.contains(suggestion), "{yaml} → {message}");
        }
    }

    #[test]
    fn compaction_threshold_must_be_finite_and_in_range() {
        let (_guard, dir) = temp_config_dir();
        for yaml in [
            "compaction_threshold: .nan\n",
            "compaction_threshold: 0\n",
            "compaction_threshold: -0.5\n",
            "compaction_threshold: 1.5\n",
        ] {
            std::fs::write(dir.path().join("config.yaml"), yaml).unwrap();
            let err = Config::load(Path::new("/tmp/project")).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains("compaction_threshold"),
                "{yaml} → {message}"
            );
            assert!(message.contains("0.8"), "{yaml} → {message}");
        }
        // 边界值 1.0 合法。
        std::fs::write(
            dir.path().join("config.yaml"),
            "compaction_threshold: 1.0\n",
        )
        .unwrap();
        let config = Config::load(Path::new("/tmp/project")).unwrap();
        assert!((config.compaction_threshold - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn empty_model_provider_shell_and_plugin_entries_rejected() {
        let (_guard, dir) = temp_config_dir();
        let cases = [
            ("model: \"\"\n", "model"),
            ("provider: \"   \"\n", "provider"),
            ("shell: \"\"\n", "shell"),
            ("plugins:\n  - \"\"\n", "plugins[0]"),
        ];
        for (yaml, field) in cases {
            std::fs::write(dir.path().join("config.yaml"), yaml).unwrap();
            let err = Config::load(Path::new("/tmp/project")).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(field), "{yaml} → {message}");
            assert!(message.contains("config.yaml"), "{yaml} → {message}");
        }
    }

    #[test]
    fn env_override_with_blank_value_is_attributed_to_env_var() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(dir.path().join("config.yaml"), "model: gpt-5\n").unwrap();
        std::env::set_var("INSTAGENT_MODEL", "   ");
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("INSTAGENT_MODEL"), "{message}");
        assert!(message.contains("model"), "{message}");
        std::env::remove_var("INSTAGENT_MODEL");
    }

    // ---- todo 08 / D03：下溢阈值、有界读取与合并后校验 ----

    #[test]
    fn underflow_threshold_narrowing_to_zero_is_rejected() {
        let (_guard, dir) = temp_config_dir();
        // 1e-46 在 f64 合法但 f32 下溢到 0（f32 最小次正规约 1.4e-45）：
        // 必须拒绝，不退化成永不压缩。
        std::fs::write(
            dir.path().join("config.yaml"),
            "compaction_threshold: 1e-46\n",
        )
        .unwrap();
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("compaction_threshold"), "{message}");
        assert!(message.contains("0.8"), "{message}");
    }

    #[test]
    fn oversized_config_is_rejected_with_source_and_left_unchanged() {
        let (_guard, dir) = temp_config_dir();
        let path = dir.path().join("config.yaml");
        // 合法前缀 + 填充到超限 1 字节：错误带来源、不回显原文、文件不动。
        let filler = "x".repeat(MAX_CONFIG_FILE_BYTES as usize + 1);
        let content = format!("provider: openai\nmodel: m\nshell: /bin/sh # {filler}\n");
        assert!(content.len() as u64 > MAX_CONFIG_FILE_BYTES);
        std::fs::write(&path, &content).unwrap();
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("config.yaml"), "{message}");
        assert!(message.contains("budget"), "{message}");
        assert!(!message.contains(&filler[..64]), "错误不得回显原文");
        assert_eq!(std::fs::read(&path).unwrap().len(), content.len());
    }

    #[test]
    fn validate_merged_rejects_blank_cli_model_override() {
        let (_guard, _dir) = temp_config_dir();
        let config = Config {
            provider: Some("fake".into()),
            model: Some("   ".into()),
            ..Config::default()
        };
        let err = config
            .validate_merged("merged config (config.yaml + env + CLI -m/--model)")
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("model"), "{message}");
        assert!(message.contains("CLI -m"), "{message}");
    }

    #[test]
    fn type_error_points_at_source_file() {
        let (_guard, dir) = temp_config_dir();
        std::fs::write(dir.path().join("config.yaml"), "max_tokens: abc\n").unwrap();
        let err = Config::load(Path::new("/tmp/project")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("config.yaml"), "{message}");
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
