//! provider 装配：插件 JSON → engine 实例（第三版 §2.4；模块表见第三版 §4）。
//!
//! 扫描启用插件（含 `07` bundled）的 `<plugin>/<NAMESPACE>/providers/*.json`，
//! 解析成 [`ProviderDef`]（K1：变量展开 `${env:NAME}` / `${PLUGIN_ROOT}` /
//! `${PLUGIN_DATA}`，`${PORT}` 原样保留给 `11` 拉起时展开）。
//! 按名字查找（K2）：重名报错并要求写成 `plugin/name`；用户插件覆盖 bundled。
//! engine 分派：`openai` → `09`；`proxy` → `11` 的 [`ProxyProvider`]（拉起 +
//! 就绪轮询）。
//! `context_limit` 四级顺序：配置覆盖 → provider models 表 → `08` 前缀小表 → 128k；
//! provider 找不到/歧义时不再静默降级，返回带 provider/model/source 的 warning
//! note（todo 08 / R13）。provider JSON 读取有大小上限（[`MAX_PROVIDER_JSON_BYTES`]）。

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::plugin::install::data_dir;
use crate::plugin::install::plugin_data_dir_at;
use crate::plugin::PluginSet;
use crate::plugin::PluginSource;
use crate::plugin::NAMESPACE;
use crate::provider::context_limit_for;
use crate::provider::openai::OpenAiProvider;
use crate::provider::proxy::ProxyProvider;
use crate::provider::EngineKind;
use crate::provider::Provider;
use crate::provider::ProviderDef;

/// 插件命名空间下存放 provider JSON 的子目录（第三版 §2.1）。
const PROVIDERS_DIR: &str = "providers";

/// provider JSON 的读取上限（todo 08 / R9 / S18）：定义文件本身很小，
/// 超限文件在装载边界拒绝，避免坏输入拖垮解析与错误输出。
pub const MAX_PROVIDER_JSON_BYTES: u64 = 1024 * 1024;

/// 带大小上限读 provider JSON：超限报错指出文件、实际大小与上限。
fn read_provider_json(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("read {}", path.display()))?
        .len();
    anyhow::ensure!(
        size <= MAX_PROVIDER_JSON_BYTES,
        "provider file {} is too large: {size} bytes exceeds the {MAX_PROVIDER_JSON_BYTES} byte limit",
        path.display()
    );
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// 一个 provider 定义 + 它的来源插件（覆盖与消歧的依据）。
/// `root` 供 `11` 解析 proxy 的 `./` 相对命令（约定同 `06`）。
#[derive(Debug, Clone)]
struct Entry {
    def: ProviderDef,
    plugin: String,
    source: PluginSource,
    root: PathBuf,
}

impl Entry {
    fn qualified(&self) -> String {
        format!("{}/{}", self.plugin, self.def.name)
    }
}

/// 全部可用 provider 定义（来自启用插件）。
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    entries: Vec<Entry>,
}

impl ProviderRegistry {
    /// 扫描 [`PluginSet`]（含 bundled）所有插件的 provider JSON 并装配。
    pub fn from_plugins(plugins: &PluginSet) -> Result<Self> {
        Self::from_plugins_at(plugins, &data_dir()?)
    }

    /// [`Self::from_plugins`] 的路径层：`PLUGIN_DATA` 的数据根由参数给出
    /// （约定同 `07` 的 `load_bundled_at`，测试不必改写进程全局
    /// `INSTAGENT_DATA_DIR`；全 crate 测试共用 `config::lock_env` 一把锁）。
    pub(crate) fn from_plugins_at(plugins: &PluginSet, data_base: &Path) -> Result<Self> {
        let mut entries = Vec::new();
        for plugin in plugins.iter() {
            let dir = plugin.root.join(NAMESPACE).join(PROVIDERS_DIR);
            let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
                Ok(read) => read
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
                    .collect(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
            };
            paths.sort();
            let plugin_data = plugin_data_dir_at(data_base, &plugin.manifest.name)?;
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for path in paths {
                let text = read_provider_json(&path)?;
                let def = parse_provider_def(&text, &plugin.root, &plugin_data)
                    .with_context(|| format!("in {}", path.display()))?;
                def.validate()
                    .with_context(|| format!("in {}", path.display()))?;
                if !seen.insert(def.name.clone()) {
                    bail!(
                        "plugin `{}` declares provider `{}` twice",
                        plugin.manifest.name,
                        def.name
                    );
                }
                entries.push(Entry {
                    def,
                    plugin: plugin.manifest.name.clone(),
                    source: plugin.source,
                    root: plugin.root.clone(),
                });
            }
        }
        Ok(Self { entries })
    }

    /// 按名字查找并构造引擎实例；重名报错并要求写 `plugin/name`。
    pub async fn get(&self, name: &str) -> Result<Arc<dyn Provider>> {
        let entry = self.resolve(name)?;
        match entry.def.engine {
            EngineKind::Openai => Ok(Arc::new(OpenAiProvider::new(&entry.def)?)),
            EngineKind::Proxy => {
                let provider = ProxyProvider::start(&entry.def, &entry.root)
                    .await
                    .with_context(|| format!("provider `{}`", entry.qualified()))?;
                Ok(Arc::new(provider))
            }
        }
    }

    /// 按名字解析出定义（重名报错 / bundled 覆盖 / `plugin/name` 消歧）。
    pub fn lookup(&self, name: &str) -> Result<&ProviderDef> {
        Ok(&self.resolve(name)?.def)
    }

    /// 定义该 provider 的插件名。
    pub fn provider_plugin(&self, name: &str) -> Result<String> {
        Ok(self.resolve(name)?.plugin.clone())
    }

    /// 全部可用名（错误提示 / 补全用）；重名的裸名同时给出 `plugin/name` 形态。
    pub fn names(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = self.entries.iter().map(|e| e.def.name.clone()).collect();
        for entry in &self.entries {
            if self.bare_matches(&entry.def.name).len() > 1 {
                names.insert(entry.qualified());
            }
        }
        names.into_iter().collect()
    }

    /// 该 (provider, model) 的上下文上限（四级顺序）：配置覆盖 → provider
    /// models 表 → `08` 前缀小表 → 128k（后两级由 [`context_limit_for`] 兜底）。
    ///
    /// 返回 `(limit, warnings)`：provider 找不到或歧义时不再静默降级到前缀小表，
    /// 而是返回一条带 provider / model / 来源的 warning（R13 / todo 08）。
    pub fn context_limit(
        &self,
        provider: &str,
        model: &str,
        config: &Config,
    ) -> (u32, Vec<String>) {
        if let Some(limit) = config.context_limit {
            return (limit, Vec::new());
        }
        match self.lookup(provider) {
            Ok(def) => {
                if let Some(model_def) = def.models.iter().find(|m| m.name == model) {
                    if let Some(limit) = model_def.context_limit {
                        return (limit, Vec::new());
                    }
                }
                (context_limit_for(model), Vec::new())
            }
            Err(err) => {
                let limit = context_limit_for(model);
                let warning = format!(
                    "context limit for provider `{provider}` / model `{model}` resolved from the \
                     model-prefix fallback table (source: prefix table, limit {limit}), because: {err}; \
                     set config `context_limit` or pick a known provider"
                );
                (limit, vec![warning])
            }
        }
    }

    /// 查找规则：`plugin/name` 精确匹配；裸名先剔除被用户插件覆盖的 bundled
    /// 候选，仍剩多个则要求消歧（第三版 §2.4）。
    fn resolve(&self, name: &str) -> Result<&Entry> {
        let matches = match name.split_once('/') {
            Some((plugin, provider)) => self
                .entries
                .iter()
                .filter(|e| e.plugin == plugin && e.def.name == provider)
                .collect::<Vec<_>>(),
            None => {
                let all = self.bare_matches(name);
                let user = all
                    .iter()
                    .copied()
                    .filter(|e| e.source != PluginSource::Bundled)
                    .collect::<Vec<_>>();
                if user.is_empty() {
                    all
                } else {
                    user
                }
            }
        };
        match matches.len() {
            1 => Ok(matches[0]),
            0 => bail!(
                "unknown provider `{name}` (available: {})",
                self.names().join(", ")
            ),
            _ => bail!(
                "provider `{name}` is defined by multiple plugins ({}); write it as `plugin/name`",
                matches
                    .iter()
                    .map(|e| e.qualified())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn bare_matches(&self, name: &str) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.def.name == name).collect()
    }
}

/// 解析单个 provider JSON：反序列化 → 变量展开 → 形状检查。
/// 签名比 00 空壳多了 plugin_root / plugin_data 两个入参：`${PLUGIN_ROOT}` /
/// `${PLUGIN_DATA}` 的展开离不开它们。
pub fn parse_provider_def(
    json: &str,
    plugin_root: &Path,
    plugin_data: &Path,
) -> Result<ProviderDef> {
    let mut value: Value = serde_json::from_str(json).context("provider JSON is not valid JSON")?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<missing name>")
        .to_string();
    expand_vars(&mut value, plugin_root, plugin_data)
        .with_context(|| format!("provider `{name}`"))?;
    let def: ProviderDef = serde_json::from_value(value)
        .with_context(|| format!("invalid provider definition `{name}`"))?;
    Ok(def)
}

/// provider JSON 的变量展开：`${env:NAME}`、`${PLUGIN_ROOT}`、`${PLUGIN_DATA}`；
/// 单次、非递归（替换结果不再扫描），其余 `${...}`（含 `${PORT}`，留给 `11`）
/// 原样保留。未定义的环境变量报错并指出变量名（provider 名由调用方补上下文）。
pub fn expand_vars(value: &mut Value, plugin_root: &Path, plugin_data: &Path) -> Result<()> {
    match value {
        Value::Object(map) => {
            for item in map.values_mut() {
                expand_vars(item, plugin_root, plugin_data)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                expand_vars(item, plugin_root, plugin_data)?;
            }
        }
        Value::String(text) => *text = expand_str(text, plugin_root, plugin_data)?,
        _ => {}
    }
    Ok(())
}

fn expand_str(text: &str, plugin_root: &Path, plugin_data: &Path) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let token = &rest[start + 2..start + end];
        match token {
            "PLUGIN_ROOT" => out.push_str(&plugin_root.display().to_string()),
            "PLUGIN_DATA" => out.push_str(&plugin_data.display().to_string()),
            _ => match token.strip_prefix("env:") {
                Some(var) => {
                    let value = std::env::var(var)
                        .with_context(|| format!("env var `{var}` is not set"))?;
                    out.push_str(&value);
                }
                // 未知 `${...}`（如 `${PORT}`）原样保留。
                None => out.push_str(&rest[start..start + end + 1]),
            },
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use crate::plugin::PluginManifest;
    use std::collections::BTreeMap;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    struct Env {
        _guard: MutexGuard<'static, ()>,
        data: TempDir,
    }

    /// 串行化进程级环境变量（约定同 `discovery.rs` / `bundled.rs`）；
    /// PLUGIN_DATA 根走 `from_plugins_at` 参数，不碰 `INSTAGENT_DATA_DIR`。
    fn isolated() -> Env {
        Env {
            _guard: crate::config::lock_env(),
            data: TempDir::new().unwrap(),
        }
    }

    fn plugin(root: PathBuf, name: &str, source: PluginSource) -> Plugin {
        std::fs::create_dir_all(root.join(NAMESPACE).join(PROVIDERS_DIR)).unwrap();
        Plugin {
            manifest: PluginManifest {
                schema: None,
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
                author: None,
                homepage: None,
                repository: None,
                license: None,
                keywords: vec![],
                extensions: BTreeMap::new(),
            },
            root,
            source,
        }
    }

    fn write_provider(plugin: &Plugin, file: &str, content: &str) {
        let path = plugin.root.join(NAMESPACE).join(PROVIDERS_DIR).join(file);
        std::fs::write(path, content).unwrap();
    }

    fn registry(env: &Env, plugins: &[Plugin]) -> ProviderRegistry {
        let set = PluginSet {
            plugins: plugins.to_vec(),
            skipped: vec![],
        };
        ProviderRegistry::from_plugins_at(&set, env.data.path()).expect("from_plugins_at")
    }

    fn def_json(name: &str, engine: &str, base_url: &str) -> String {
        format!(r#"{{"name":"{name}","engine":"{engine}","base_url":"{base_url}","headers":{{}}}}"#)
    }

    #[test]
    fn expands_env_plugin_root_plugin_data_and_keeps_port() {
        let _env = isolated();
        std::env::set_var("REG_TEST_BASE", "https://api.example.com");
        std::env::set_var("REG_TEST_KEY", "s3cret");
        let root = Path::new("/opt/plugin-root");
        let data = Path::new("/opt/plugin-data");
        let text = r#"{
            "name": "acme",
            "engine": "openai",
            "base_url": "${env:REG_TEST_BASE}/v1",
            "headers": {
                "authorization": "Bearer ${env:REG_TEST_KEY}",
                "x-root": "${PLUGIN_ROOT}",
                "x-data": "${PLUGIN_DATA}",
                "x-port": "${PORT}"
            },
            "models": [{ "name": "m-${env:REG_TEST_KEY}" }]
        }"#;
        let def = parse_provider_def(text, root, data).unwrap();
        assert_eq!(def.base_url.as_deref(), Some("https://api.example.com/v1"));
        assert_eq!(
            def.headers.get("authorization").map(String::as_str),
            Some("Bearer s3cret")
        );
        assert_eq!(
            def.headers.get("x-root").map(String::as_str),
            Some("/opt/plugin-root")
        );
        assert_eq!(
            def.headers.get("x-data").map(String::as_str),
            Some("/opt/plugin-data")
        );
        // ${PORT} 留给 11 拉起时展开，其余未知 ${...} 也原样保留。
        assert_eq!(
            def.headers.get("x-port").map(String::as_str),
            Some("${PORT}")
        );
        assert_eq!(def.models[0].name, "m-s3cret");
        std::env::remove_var("REG_TEST_BASE");
        std::env::remove_var("REG_TEST_KEY");
    }

    #[test]
    fn undefined_env_var_names_provider_and_variable() {
        let env = isolated();
        std::env::remove_var("REG_TEST_UNDEFINED");
        let p = plugin(env.data.path().join("p1"), "p1", PluginSource::User);
        write_provider(
            &p,
            "acme.json",
            &def_json("acme", "openai", "${env:REG_TEST_UNDEFINED}"),
        );
        let set = PluginSet {
            plugins: vec![p],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("acme"), "{message}");
        assert!(message.contains("REG_TEST_UNDEFINED"), "{message}");
    }

    #[test]
    fn duplicate_provider_requires_plugin_qualified_name() {
        let env = isolated();
        let alpha = plugin(env.data.path().join("alpha"), "alpha", PluginSource::User);
        let beta = plugin(env.data.path().join("beta"), "beta", PluginSource::Project);
        write_provider(
            &alpha,
            "groq.json",
            &def_json("groq", "openai", "https://a.test/v1"),
        );
        write_provider(
            &beta,
            "groq.json",
            &def_json("groq", "openai", "https://b.test/v1"),
        );
        let registry = registry(&env, &[alpha, beta]);

        let err = registry.lookup("groq").unwrap_err().to_string();
        assert!(err.contains("multiple plugins"), "{err}");
        assert!(
            err.contains("alpha/groq") && err.contains("beta/groq"),
            "{err}"
        );
        assert_eq!(
            registry.lookup("alpha/groq").unwrap().base_url.as_deref(),
            Some("https://a.test/v1")
        );
        assert_eq!(
            registry.lookup("beta/groq").unwrap().base_url.as_deref(),
            Some("https://b.test/v1")
        );
        // names() 同时给出裸名与两个消歧形态。
        assert_eq!(
            registry.names(),
            ["alpha/groq", "beta/groq", "groq"].map(String::from)
        );
    }

    #[test]
    fn user_plugin_overrides_bundled() {
        let env = isolated();
        let bundled = plugin(
            env.data.path().join("bundled"),
            "bundled",
            PluginSource::Bundled,
        );
        let user = plugin(env.data.path().join("user"), "my-groq", PluginSource::User);
        write_provider(
            &bundled,
            "groq.json",
            r#"{"name":"groq","engine":"openai","display_name":"Groq (bundled)","base_url":"https://bundled.test/v1","headers":{}}"#,
        );
        write_provider(
            &user,
            "groq.json",
            r#"{"name":"groq","engine":"openai","display_name":"Groq (mine)","base_url":"https://user.test/v1","headers":{}}"#,
        );
        let registry = registry(&env, &[bundled, user]);
        // 用户插件覆盖 bundled：裸名解析到 user 的 base_url。
        assert_eq!(
            registry.lookup("groq").unwrap().base_url.as_deref(),
            Some("https://user.test/v1")
        );
        // 显式点名 bundled 仍然可达。
        assert_eq!(
            registry.lookup("bundled/groq").unwrap().base_url.as_deref(),
            Some("https://bundled.test/v1")
        );
    }

    #[test]
    fn context_limit_follows_four_level_order() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(
            &p,
            "svc.json",
            r#"{"name":"svc","engine":"openai","base_url":"https://svc.test/v1","headers":{},
               "models":[{"name":"known-model","context_limit":555},{"name":"no-limit"}]}"#,
        );
        let registry = registry(&env, &[p]);

        // 1) 配置覆盖最高，且无 warning。
        let config = Config {
            context_limit: Some(999),
            ..Config::default()
        };
        assert_eq!(
            registry.context_limit("svc", "known-model", &config),
            (999, vec![])
        );
        // 2) provider models 表。
        let config = Config::default();
        assert_eq!(
            registry.context_limit("svc", "known-model", &config),
            (555, vec![])
        );
        // 3) 表里有名字但没 limit / 表里没有 → 08 前缀小表（已知 provider 不告警）。
        assert_eq!(
            registry.context_limit("svc", "no-limit", &config),
            (128 * 1024, vec![])
        );
        assert_eq!(
            registry.context_limit("svc", "claude-sonnet-4-6", &config),
            (200 * 1024, vec![])
        );
        // 4) 前缀小表也没有 → 128k 兜底（已知 provider 不告警）。
        assert_eq!(
            registry.context_limit("svc", "mystery-9000", &config),
            (128 * 1024, vec![])
        );
    }

    #[test]
    fn context_limit_unknown_or_ambiguous_provider_warns_with_source() {
        let env = isolated();
        let alpha = plugin(env.data.path().join("alpha"), "alpha", PluginSource::User);
        let beta = plugin(env.data.path().join("beta"), "beta", PluginSource::User);
        write_provider(
            &alpha,
            "dup.json",
            &def_json("dup", "openai", "https://a.test/v1"),
        );
        write_provider(
            &beta,
            "dup.json",
            &def_json("dup", "openai", "https://b.test/v1"),
        );
        let registry = registry(&env, &[alpha, beta]);
        let config = Config::default();

        // 未知 provider：limit 仍来自前缀小表，但带 provider/model/source 的告警。
        let (limit, warnings) = registry.context_limit("absent", "mystery-9000", &config);
        assert_eq!(limit, 128 * 1024);
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains("absent"), "{warning}");
        assert!(warning.contains("mystery-9000"), "{warning}");
        assert!(warning.contains("prefix table"), "{warning}");
        assert!(warning.contains("unknown provider"), "{warning}");

        // 歧义（裸名多插件）同样告警而不是静默。
        let (limit, warnings) = registry.context_limit("dup", "claude-x", &config);
        assert_eq!(limit, 200 * 1024);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("multiple plugins"), "{warnings:?}");

        // 配置覆盖时即便 provider 未知也不告警（用户已显式负责）。
        let config = Config {
            context_limit: Some(4096),
            ..Config::default()
        };
        assert_eq!(
            registry.context_limit("absent", "mystery-9000", &config),
            (4096, vec![])
        );
    }

    #[tokio::test]
    async fn engine_dispatch_openai_and_proxy() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(
            &p,
            "oai.json",
            &def_json("oai", "openai", "https://oai.test/v1"),
        );
        // 11 接线：proxy 分支构造 ProxyProvider 并真正拉起（端到端拉起见
        // tests/provider_proxy.rs；这里用必然 spawn 失败的命令证明占位 bail
        // 已被接线替换）。
        write_provider(
            &p,
            "px.json",
            r#"{"name":"px","engine":"proxy","proxy":{"command":"no-such-proxy-binary-42","args":["--port","${PORT}"]}}"#,
        );
        let registry = registry(&env, &[p]);

        let provider = registry.get("oai").await.unwrap();
        assert_eq!(provider.name(), "oai");

        let err = format!("{:#}", registry.get("px").await.map(drop).unwrap_err());
        assert!(
            err.contains("spawn proxy command") && err.contains("no-such-proxy-binary-42"),
            "{err}"
        );
    }

    #[test]
    fn load_rejects_engine_section_mismatch_and_same_plugin_duplicates() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(&p, "no-proxy.json", r#"{"name":"np","engine":"proxy"}"#);
        let set = PluginSet {
            plugins: vec![p.clone()],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        // 校验错误带文件上下文，`{:#}` 给出完整错误链。
        assert!(format!("{err:#}").contains("proxy section"), "{err:#}");

        let q = plugin(env.data.path().join("q"), "q", PluginSource::User);
        write_provider(
            &q,
            "one.json",
            &def_json("dup", "openai", "https://a.test/v1"),
        );
        write_provider(
            &q,
            "two.json",
            &def_json("dup", "openai", "https://b.test/v1"),
        );
        let set = PluginSet {
            plugins: vec![q],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        assert!(
            err.to_string().contains("declares provider `dup` twice"),
            "{err:#}"
        );
    }

    #[test]
    fn oversized_provider_json_rejected_at_load_boundary() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        let huge = format!(
            r#"{{"name":"huge","engine":"openai","base_url":"https://h.test/v1","description":"{}"}}"#,
            "x".repeat(MAX_PROVIDER_JSON_BYTES as usize)
        );
        write_provider(&p, "huge.json", &huge);
        let set = PluginSet {
            plugins: vec![p],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("too large"), "{message}");
        assert!(message.contains("huge.json"), "{message}");
        assert!(
            message.contains(&MAX_PROVIDER_JSON_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn invalid_numeric_provider_fields_rejected_at_load_boundary() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(
            &p,
            "svc.json",
            r#"{"name":"svc","engine":"openai","base_url":"https://svc.test/v1","timeout_seconds":0}"#,
        );
        let set = PluginSet {
            plugins: vec![p],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("timeout_seconds"), "{message}");
        assert!(message.contains("svc"), "{message}");
        assert!(message.contains("svc.json"), "{message}");
    }

    #[test]
    fn invalid_model_fields_rejected_at_load_boundary() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(
            &p,
            "svc.json",
            r#"{"name":"svc","engine":"openai","base_url":"https://svc.test/v1",
               "models":[{"name":"m1","max_tokens":0},{"name":"m2","context_limit":0}]}"#,
        );
        let set = PluginSet {
            plugins: vec![p],
            skipped: vec![],
        };
        let err = ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("m1"), "{message}");
        assert!(message.contains("max_tokens"), "{message}");
        assert!(message.contains("svc.json"), "{message}");
    }

    #[test]
    fn bundled_providers_load_into_registry() {
        let env = isolated();
        let bundled = plugin(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bundled"),
            "bundled",
            PluginSource::Bundled,
        );
        let registry = registry(&env, &[bundled]);
        let listed = registry.names();
        let names: Vec<&str> = listed.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            ["deepseek", "groq", "ollama", "openai", "openrouter"]
        );
        // 转换约定：base_url 不带 /chat/completions，ollama 指本地。
        assert_eq!(
            registry.lookup("ollama").unwrap().base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        for name in names {
            let def = registry.lookup(name).unwrap();
            let base = def.base_url.as_deref().unwrap_or_default();
            assert!(!base.ends_with("/chat/completions"), "{name}: {base}");
        }
        assert_eq!(
            registry.context_limit(
                "groq",
                "moonshotai/kimi-k2-instruct-0905",
                &Config::default()
            ),
            (262144, vec![])
        );
        // 新形状字段（display_name / description / model max_tokens，S9）：
        // bundled JSON 携带它们，装载后可读且校验通过。
        let openai = registry.lookup("openai").unwrap();
        assert_eq!(openai.display_name.as_deref(), Some("OpenAI"));
        assert!(openai
            .description
            .as_deref()
            .unwrap()
            .contains("Chat Completions"));
        let gpt5 = openai.models.iter().find(|m| m.name == "gpt-5").unwrap();
        assert_eq!(gpt5.context_limit, Some(400000));
        assert_eq!(gpt5.max_tokens, Some(32000));
        // 旧形状字段缺省（deepseek 的 models 无 max_tokens）。
        let deepseek = registry.lookup("deepseek").unwrap();
        assert!(deepseek.models.iter().all(|m| m.max_tokens.is_none()));
    }

    #[test]
    fn unknown_provider_lists_available_names() {
        let env = isolated();
        let p = plugin(env.data.path().join("p"), "p", PluginSource::User);
        write_provider(
            &p,
            "a.json",
            &def_json("alpha", "openai", "https://a.test/v1"),
        );
        let registry = registry(&env, &[p]);
        let err = registry.lookup("nope").unwrap_err().to_string();
        assert!(err.contains("available: alpha"), "{err}");
    }
}
