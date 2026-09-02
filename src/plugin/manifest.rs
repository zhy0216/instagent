//! `plugin.json`（Agent Plugins v1.0.0）解析与校验（第三版 §2.1、§2.2）。
//!
//! 客户端不联网取 schema：`$schema` 字符串必须命中 [`PLUGIN_SCHEMA_URL`]，
//! 据此选择本地校验规则；缺失或版本不符即报错。未知顶层字段转成 warning
//! （报告但不致命）；组件目录（`skills/`、`mcp.json`、`dev.instagent/`）
//! 不存在不算错。
//! name 规则参考 goose `plugins/formats/open_plugins.rs::validate_plugin_name`
//! （只读，commit 4ad43df）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::plugin::NAMESPACE;

/// 客户端不联网取 schema，只用该字符串选择本地校验规则。
pub const PLUGIN_SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// `author` 允许纯字符串或对象（规范字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Author {
    Name(String),
    Detailed {
        name: String,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
}

/// 规范顶层十个字段；未知字段进 `unknown`，报告但不致命。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// 反域名命名空间 → 原始 JSON；客户端必须忽略自己不实现的命名空间。
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
    /// 未知顶层字段（`04` 转成 warning 列表）。
    #[serde(flatten, default)]
    pub unknown: BTreeMap<String, Value>,
}

impl PluginManifest {
    /// `extensions["dev.instagent"].minKernel`（第三版 §2.2 的小标志）；
    /// 命名空间缺失、内容不是对象或值不是字符串时为 `None`。
    pub fn min_kernel(&self) -> Option<&str> {
        self.extensions
            .get(NAMESPACE)
            .and_then(|ns| ns.get("minKernel"))
            .and_then(Value::as_str)
    }
}

/// 校验产物：manifest + 非致命 warning（未知字段之类）。
#[derive(Debug, Clone)]
pub struct Validated {
    pub manifest: PluginManifest,
    pub warnings: Vec<String>,
}

/// 读 `<dir>/plugin.json` 并校验（规范 v1.0.0 位置，非 goose 草案路径）。
///
/// 组件目录是否存在不影响结果（§2.1：不存在不算错），由 `05`/`06`/`15`
/// 各自按约定位置读取。
pub fn read_manifest(dir: &Path) -> crate::Result<Validated> {
    let path = dir.join("plugin.json");
    let text =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let manifest: PluginManifest = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    validate_manifest(manifest)
}

/// `$schema` 匹配 + `name` 规则 + `extensions[NAMESPACE]` 小标志；
/// 未知顶层字段收进 warnings。
fn validate_manifest(manifest: PluginManifest) -> crate::Result<Validated> {
    match manifest.schema.as_deref() {
        None => bail!("plugin.json must declare `$schema` (expected `{PLUGIN_SCHEMA_URL}`)"),
        Some(url) if url == PLUGIN_SCHEMA_URL => {}
        Some(url) => {
            bail!("unsupported `$schema` version `{url}` (expected `{PLUGIN_SCHEMA_URL}`)")
        }
    }

    validate_plugin_name(&manifest.name)?;

    let mut warnings = Vec::new();
    for key in manifest.unknown.keys() {
        warnings.push(format!(
            "ignoring unknown top-level field `{key}` in plugin.json"
        ));
    }

    if let Some(ext) = manifest.extensions.get(NAMESPACE) {
        match ext {
            Value::Object(map) => {
                if map.get("minKernel").is_some_and(|v| !v.is_string()) {
                    warnings.push(format!(
                        "ignoring non-string `extensions[\"{NAMESPACE}\"].minKernel`"
                    ));
                }
            }
            _ => warnings.push(format!(
                "ignoring `extensions[\"{NAMESPACE}\"]`: not a JSON object"
            )),
        }
    }

    Ok(Validated { manifest, warnings })
}

/// name：1~64 字符，小写字母数字与 `-` `.`，首尾字母数字，不含 `--` `..`。
pub fn validate_plugin_name(name: &str) -> crate::Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("invalid plugin name `{name}`: names must be 1-64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        bail!(
            "invalid plugin name `{name}`: only lowercase letters, digits, `-` and `.` are allowed"
        );
    }
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !name.starts_with(alnum) || !name.ends_with(alnum) {
        bail!("invalid plugin name `{name}`: must start and end with a letter or digit");
    }
    if name.contains("--") || name.contains("..") {
        bail!("invalid plugin name `{name}`: must not contain `--` or `..`");
    }
    Ok(())
}

/// 插件内组件命名 `<plugin>:<component>`（goose namespaced_component_name）。
pub fn namespaced_component_name(plugin: &str, component: &str) -> String {
    format!("{plugin}:{component}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/manifest")
            .join(name)
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    /// 把 fixture 目录整体拷进 tempdir（保留组件目录）。
    fn plugin_dir(fixture: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        copy_dir(&fixture_path(fixture), tmp.path());
        tmp
    }

    /// 只写一份 plugin.json 到 tempdir（组件目录缺失）。
    fn dir_with_manifest(json: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("plugin.json"), json).unwrap();
        tmp
    }

    fn manifest_of(fixture: &str) -> String {
        fs::read_to_string(fixture_path(fixture).join("plugin.json")).unwrap()
    }

    #[test]
    fn valid_manifest_with_components_passes() {
        let dir = plugin_dir("valid");
        let validated = read_manifest(dir.path()).unwrap();
        let m = &validated.manifest;

        assert!(validated.warnings.is_empty(), "warnings: {validated:?}");
        assert_eq!(
            m.schema.as_deref(),
            Some("https://agent-plugins.org/schemas/1.0.0/plugin.schema.json")
        );
        assert_eq!(m.name, "groq-and-review");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(
            m.description.as_deref(),
            Some("Groq provider + a review skill + a lint hook")
        );
        assert_eq!(
            m.author,
            Some(Author::Detailed {
                name: "Instagent Team".into(),
                email: Some("team@instagent.dev".into()),
                url: None,
            })
        );
        assert_eq!(m.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(m.keywords, vec!["groq".to_string(), "review".to_string()]);
        assert_eq!(m.min_kernel(), Some("0.1"));
        assert!(m.extensions.contains_key("com.example.tools"));
        assert!(m.unknown.is_empty());
    }

    #[test]
    fn invalid_name_rejected() {
        let dir = plugin_dir("invalid-name");
        let err = read_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid plugin name"), "{err}");
        assert!(dir.path().join("plugin.json").is_file());
    }

    #[test]
    fn schema_version_mismatch_rejected() {
        let dir = plugin_dir("bad-schema");
        let err = read_manifest(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported `$schema` version"), "{msg}");
        assert!(msg.contains("0.9.0"), "{msg}");
    }

    #[test]
    fn missing_schema_field_rejected() {
        let json = manifest_of("bad-schema").replace(
            "\"$schema\": \"https://agent-plugins.org/schemas/0.9.0/plugin.schema.json\",",
            "",
        );
        let dir = dir_with_manifest(&json);
        let err = read_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("must declare `$schema`"), "{err}");
    }

    #[test]
    fn unknown_fields_warn_but_not_fatal() {
        let dir = plugin_dir("unknown-fields");
        let validated = read_manifest(dir.path()).unwrap();
        assert_eq!(validated.manifest.name, "warned-plugin");
        assert_eq!(validated.warnings.len(), 2, "{:?}", validated.warnings);
        assert!(validated
            .warnings
            .iter()
            .any(|w| w.contains("experimental")));
        assert!(validated.warnings.iter().any(|w| w.contains("vendorExt")));
        assert_eq!(validated.manifest.unknown["vendorExt"], Value::from("acme"));
    }

    #[test]
    fn missing_component_dirs_are_ok() {
        let dir = plugin_dir("components-missing");
        let validated = read_manifest(dir.path()).unwrap();
        assert_eq!(validated.manifest.name, "bare-plugin");
        assert!(validated.warnings.is_empty());
        assert_eq!(validated.manifest.min_kernel(), None);
    }

    #[test]
    fn bad_namespace_extension_and_minkernel_warn_only() {
        let json = manifest_of("components-missing").replace(
            "\"version\": \"0.1.0\",",
            "\"version\": \"0.1.0\",\n  \"extensions\": {\n    \"dev.instagent\": 42\n  },",
        );
        let dir = dir_with_manifest(&json);
        let validated = read_manifest(dir.path()).unwrap();
        assert_eq!(validated.warnings.len(), 1, "{:?}", validated.warnings);
        assert!(validated.warnings[0].contains("not a JSON object"));
        assert_eq!(validated.manifest.min_kernel(), None);

        let json = manifest_of("components-missing").replace(
            "\"version\": \"0.1.0\",",
            "\"version\": \"0.1.0\",\n  \"extensions\": {\n    \"dev.instagent\": { \"minKernel\": 1 } }\n,",
        );
        let dir = dir_with_manifest(&json);
        let validated = read_manifest(dir.path()).unwrap();
        assert_eq!(validated.warnings.len(), 1, "{:?}", validated.warnings);
        assert!(validated.warnings[0].contains("minKernel"));
        assert_eq!(validated.manifest.min_kernel(), None);
    }

    #[test]
    fn missing_plugin_json_fails() {
        let tmp = TempDir::new().unwrap();
        let err = read_manifest(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("plugin.json"), "{err}");
    }

    #[test]
    fn unparsable_plugin_json_fails() {
        let dir = dir_with_manifest("{ not json");
        let err = read_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"), "{err}");
    }

    #[test]
    fn name_rules() {
        for ok in [
            "a",
            "0",
            "my-plugin",
            "my.plugin",
            "a1.b2-c3",
            &"a".repeat(64),
        ] {
            validate_plugin_name(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "",
            "My-Plugin",
            "my_plugin",
            "-lead",
            "trail-",
            ".dot",
            "dash--tie",
            "dot..tie",
            "插件",
            &"a".repeat(65),
        ] {
            assert!(
                validate_plugin_name(bad).is_err(),
                "{bad} should be invalid"
            );
        }
    }

    #[test]
    fn namespaced_component_name_joins_with_colon() {
        assert_eq!(
            namespaced_component_name("groq-and-review", "review"),
            "groq-and-review:review"
        );
    }
}
