//! `plugin.json`（Agent Plugins v1.0.0）解析与校验（第三版 §2.1、§2.2；规范 §5、§8）。
//!
//! 客户端不联网取 schema：`$schema` 字符串必须命中 [`PLUGIN_SCHEMA_URL`]，
//! 据此选择本地校验规则；缺失或版本不符即报错。
//!
//! 字段校验口径（§5.2：除两类非致命例外外，任何 schema 违规都致命）：
//! - **致命**（安装/发现阶段拒绝插件）：顶层不是对象；`$schema`/`name`/`version`
//!   缺失、类型不符或 `version` 为空；`description`/`author`/`homepage`/
//!   `repository`/`license`/`keywords` 类型不符；`author` 对象含 `name`/`email`/
//!   `url` 之外的字段；`extensions` 的命名空间键不是反域名或其值不是对象。
//! - **非致命**（报告并忽略，§5.2、§8.1）：未知顶层字段 → `tracing::warn!` 后
//!   丢弃；`extensions` 整体不是对象 → `tracing::warn!` 后按缺省处理。客户端
//!   不得给未知字段赋语义。显式 `null` 一律按“字段未出现”处理。
//! - **不做**（§5.4 明文 MUST NOT 拒绝）：`version` 非 SemVer、`homepage`/
//!   `repository`/`author.url` 不是规范 URL、`author.email` 不是邮箱、
//!   `license` 不是 SPDX。其中 `version` 仍是必填的非空字符串（本仓库 CLI 展示
//!   与 update 记录依赖它），非 SemVer 只 `tracing::warn!`。
//!
//! 每个错误都带 `plugin.json` 路径、插件名、字段名与建议值（与 `08` 的
//! [`crate::provider::ProviderDef::validate`] 同一错误模型）；读取使用硬上限
//! [`MAX_MANIFEST_BYTES`]（与 `10` 的 `mcp.json` 1 MiB 同口径），解析失败时输入
//! 只回显截断摘要，不把无界原始 JSON 灌进日志。
//! name 规则参考 goose `plugins/formats/open_plugins.rs::validate_plugin_name`
//! （只读，commit 4ad43df）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context};
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value};

/// 客户端不联网取 schema，只用该字符串选择本地校验规则。
pub const PLUGIN_SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// `plugin.json` 单次读取的硬上限（1 MiB，与 `mcp.json` 的
/// [`crate::plugin::mcp_config::MAX_MCP_CONFIG_BYTES`] 同口径）：超限直接拒绝，
/// 不把坏文件读进内存或错误日志。
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// 回显进错误消息里的用户输入的最大字符数，超出截断加省略号。
const MAX_ECHO_CHARS: usize = 120;

/// 规范 §5.2 的封闭顶层字段集；其余字段报告并忽略。
const KNOWN_FIELDS: [&str; 10] = [
    "$schema",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "extensions",
];

/// 本客户端的扩展命名空间（§8.1：其内容规则由客户端自定）。
const INSTAGENT_NAMESPACE: &str = "dev.instagent";

/// 错误前缀的构造口径：来源文件 + 插件名。
type Source<'a> = &'a dyn Fn() -> String;

fn brief(value: &str) -> String {
    let mut out: String = value.chars().take(MAX_ECHO_CHARS).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
}

/// `类型 + 值摘要`：错误里说明“实际拿到了什么”，值部分有界。
fn got(value: &Value) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("{kind} `{}`", brief(&value.to_string()))
}

fn semver_regex() -> &'static Regex {
    static SEMVER: OnceLock<Regex> = OnceLock::new();
    SEMVER.get_or_init(|| {
        // SemVer 2.0 官方参考正则（semver.org），无 semver 依赖可用（依赖由 01 锁定）。
        let parts: [&str; 4] = [
            r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)",
            r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)",
            r"(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?",
            r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$",
        ];
        Regex::new(&parts.concat()).expect("semver pattern is valid")
    })
}

/// 反域名命名空间：至少两段，段内 `[A-Za-z0-9-]` 且非空（§3、§8）。
fn namespace_regex() -> &'static Regex {
    static NAMESPACE: OnceLock<Regex> = OnceLock::new();
    NAMESPACE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$").expect("namespace pattern is valid")
    })
}

/// `dev.instagent.minKernel` 之类的小版本号：点分数字（`1`、`0.1`、`0.1.0`）。
fn dotted_version_regex() -> &'static Regex {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    VERSION.get_or_init(|| Regex::new(r"^\d+(\.\d+)*$").expect("dotted version pattern is valid"))
}

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

/// 规范顶层十个字段；未知字段报告后忽略（见模块文档的兼容策略）。
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
}

/// 读 `<dir>/plugin.json` 并校验（规范 v1.0.0 位置，非 goose 草案路径）。
///
/// 组件目录是否存在不影响结果（§2.1：不存在不算错），由 `05`/`06`/`15`
/// 各自按约定位置读取。
pub fn read_manifest(dir: &Path) -> crate::Result<PluginManifest> {
    let path = dir.join("plugin.json");
    let text = read_bounded(&path)?;
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        anyhow::anyhow!(
            "Failed to parse {}: {err}; input is {} bytes, begins `{}`",
            path.display(),
            text.len(),
            brief(&text)
        )
    })?;
    let manifest = validate_fields(&path, value)?;
    validate_manifest(manifest, &path)
}

/// 带硬上限的 manifest 读取：先查 metadata 再读，超限错误指出路径与上限。
fn read_bounded(path: &Path) -> crate::Result<String> {
    let size = fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    if size > MAX_MANIFEST_BYTES {
        bail!(
            "{}: plugin manifest is {size} bytes, over the {MAX_MANIFEST_BYTES} byte limit",
            path.display()
        );
    }
    fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))
}

/// 字段级校验（类型形状 + 跨字段约束），通过后产出类型化 manifest。
///
/// 逐字段在 [`Value`] 上检查，错误才能指到字段名；通过后的 serde 反序列化不该
/// 再失败（失败即说明这里漏了规则，`context` 留兜底信息）。
fn validate_fields(path: &Path, value: Value) -> crate::Result<PluginManifest> {
    let mut fields = match value {
        Value::Object(fields) => fields,
        other => bail!(
            "{}: plugin.json must contain a JSON object at the top level, got {}",
            path.display(),
            got(&other)
        ),
    };

    let name_label = match fields.get("name") {
        None | Some(Value::Null) => "<missing>".to_string(),
        Some(Value::String(name)) => brief(name),
        Some(other) => format!("<non-string name: {}>", brief(&other.to_string())),
    };
    let source = || format!("{}: plugin `{name_label}`", path.display());

    // 显式 `null` 按“字段未出现”处理（缺省语义），避免落到 serde 的类型报错。
    for key in KNOWN_FIELDS {
        if matches!(fields.get(key), Some(Value::Null)) {
            fields.remove(key);
        }
    }

    for key in fields.keys() {
        if !KNOWN_FIELDS.contains(&key.as_str()) {
            // §5.2：报告并忽略，不赋语义、不致命。
            tracing::warn!("{}: ignoring unknown field `{}`", source(), brief(key));
        }
    }

    match fields.get("$schema") {
        None | Some(Value::String(_)) => {}
        Some(other) => bail!(
            "{}: field `$schema` must be a string (suggested: `{PLUGIN_SCHEMA_URL}`), got {}",
            source(),
            got(other)
        ),
    }
    match fields.get("name") {
        None => bail!(
            "{}: field `name` is required (suggested: a lowercase name such as `my-plugin`)",
            source()
        ),
        Some(Value::String(_)) => {}
        Some(other) => bail!(
            "{}: field `name` must be a string, got {}",
            source(),
            got(other)
        ),
    }
    validate_version(&fields, &source)?;
    optional_string(&fields, "description", &source)?;
    validate_author(&fields, &source)?;
    optional_string(&fields, "homepage", &source)?;
    optional_string(&fields, "repository", &source)?;
    optional_string(&fields, "license", &source)?;
    validate_keywords(&fields, &source)?;
    validate_extensions(&mut fields, &source)?;

    let manifest: PluginManifest = serde_json::from_value(Value::Object(fields))
        .with_context(|| format!("{}: manifest shape check missed a field", source()))?;
    Ok(manifest)
}

/// `version`：必填非空字符串；非 SemVer 只警告（§5.4 不允许因此拒绝插件）。
fn validate_version(fields: &Map<String, Value>, source: Source) -> crate::Result<()> {
    match fields.get("version") {
        None => bail!(
            "{}: field `version` is required (suggested: `1.0.0`)",
            source()
        ),
        Some(Value::String(version)) if version.trim().is_empty() => bail!(
            "{}: field `version` must not be empty (suggested: `1.0.0`)",
            source()
        ),
        Some(Value::String(version)) => {
            if !semver_regex().is_match(version) {
                tracing::warn!(
                    "{}: field `version` `{}` is not valid Semantic Versioning; \
                     update checks and cache freshness may misbehave",
                    source(),
                    brief(version)
                );
            }
            Ok(())
        }
        Some(other) => bail!(
            "{}: field `version` must be a string (suggested: `1.0.0`), got {}",
            source(),
            got(other)
        ),
    }
}

/// `author`：字符串，或只含 `name`/`email`/`url` 字符串字段的对象。
/// §5.4：对象里任何其它字段或值类型都使 manifest 无效。
fn validate_author(fields: &Map<String, Value>, source: Source) -> crate::Result<()> {
    match fields.get("author") {
        None | Some(Value::String(_)) => Ok(()),
        Some(Value::Object(author)) => {
            for (key, value) in author {
                match key.as_str() {
                    "name" | "email" | "url" => {
                        if !matches!(value, Value::String(_)) {
                            bail!(
                                "{}: field `author.{key}` must be a string, got {}",
                                source(),
                                got(value)
                            );
                        }
                    }
                    other => bail!(
                        "{}: field `author` may only contain `name`, `email` and `url`, \
                         got extra field `{}`",
                        source(),
                        brief(other)
                    ),
                }
            }
            if author.contains_key("name") {
                Ok(())
            } else {
                bail!(
                    "{}: field `author` given as an object must carry `name` \
                     (suggested: `{{\"name\": \"...\"}}` or a plain string)",
                    source()
                )
            }
        }
        Some(other) => bail!(
            "{}: field `author` must be a string or an object, got {}",
            source(),
            got(other)
        ),
    }
}

fn validate_keywords(fields: &Map<String, Value>, source: Source) -> crate::Result<()> {
    match fields.get("keywords") {
        None => Ok(()),
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                if !matches!(item, Value::String(_)) {
                    bail!(
                        "{}: field `keywords[{index}]` must be a string, got {}",
                        source(),
                        got(item)
                    );
                }
            }
            Ok(())
        }
        Some(other) => bail!(
            "{}: field `keywords` must be an array of strings, got {}",
            source(),
            got(other)
        ),
    }
}

/// `extensions`：反域名命名空间 → 对象。整体不是对象时报告并忽略（§8.1 非
/// 致命）；未实现的命名空间只看形状、不验证内容（§11.1）。
fn validate_extensions(fields: &mut Map<String, Value>, source: Source) -> crate::Result<()> {
    let mut ignore = false;
    match fields.get("extensions") {
        None => return Ok(()),
        Some(Value::Object(namespaces)) => {
            for (namespace, value) in namespaces {
                if !namespace_regex().is_match(namespace) {
                    bail!(
                        "{}: field `extensions.{}` is not a reverse-domain namespace \
                         (suggested: e.g. `com.example.client`)",
                        source(),
                        brief(namespace)
                    );
                }
                if !matches!(value, Value::Object(_)) {
                    bail!(
                        "{}: field `extensions.{}` must be an object (§8.1), got {}",
                        source(),
                        brief(namespace),
                        got(value)
                    );
                }
                if namespace == INSTAGENT_NAMESPACE {
                    validate_instagent_extension(value, source)?;
                }
            }
        }
        Some(other) => {
            tracing::warn!(
                "{}: field `extensions` must be an object of namespace → object, got {}; \
                 ignoring it",
                source(),
                got(other)
            );
            ignore = true;
        }
    }
    if ignore {
        fields.insert(
            "extensions".to_string(),
            Value::Object(Map::<String, Value>::new()),
        );
    }
    Ok(())
}

/// 本客户端命名空间内已知的标志；其余键留给以后（前向兼容）。
fn validate_instagent_extension(value: &Value, source: Source) -> crate::Result<()> {
    let Some(min_kernel) = value.get("minKernel") else {
        return Ok(());
    };
    match min_kernel {
        Value::String(version) if dotted_version_regex().is_match(version) => Ok(()),
        other => bail!(
            "{}: field `extensions.{INSTAGENT_NAMESPACE}.minKernel` must be a dotted version \
             string (suggested: `0.1`), got {}",
            source(),
            got(other)
        ),
    }
}

/// 可选字符串字段只做类型校验（§5.4：规范未加显式约束，空串合法）。
fn optional_string(fields: &Map<String, Value>, key: &str, source: Source) -> crate::Result<()> {
    match fields.get(key) {
        None | Some(Value::String(_)) => Ok(()),
        Some(other) => bail!(
            "{}: field `{key}` must be a string, got {}",
            source(),
            got(other)
        ),
    }
}

/// `$schema` 匹配 + `name` 规则（字段形状已在 [`validate_fields`] 完成）。
fn validate_manifest(manifest: PluginManifest, path: &Path) -> crate::Result<PluginManifest> {
    let source = || format!("{}: plugin `{}`", path.display(), brief(&manifest.name));
    match manifest.schema.as_deref() {
        None => bail!(
            "{}: plugin.json must declare `$schema` (expected `{PLUGIN_SCHEMA_URL}`)",
            source()
        ),
        Some(url) if url == PLUGIN_SCHEMA_URL => {}
        Some(url) => bail!(
            "{}: unsupported `$schema` version `{}` (expected `{PLUGIN_SCHEMA_URL}`)",
            source(),
            brief(url)
        ),
    }

    if let Err(err) = validate_plugin_name(&manifest.name) {
        bail!("{}: {err}", source());
    }

    Ok(manifest)
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

    /// 合法最小 manifest + 追加字段（`extra` 为 `"key": value` 形式）。
    fn manifest_json(extra: &str) -> String {
        let comma = if extra.is_empty() { "" } else { "," };
        format!(
            r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"demo","version":"1.0.0"{comma}{extra}}}"#
        )
    }

    fn load_json(json: &str) -> crate::Result<PluginManifest> {
        let dir = dir_with_manifest(json);
        read_manifest(dir.path())
    }

    fn error_of(json: &str) -> String {
        load_json(json).unwrap_err().to_string()
    }

    #[test]
    fn valid_manifest_with_components_passes() {
        let dir = plugin_dir("valid");
        let m = read_manifest(dir.path()).unwrap();

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
        assert!(m.extensions.contains_key("com.example.tools"));
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
    fn unknown_fields_tolerated_not_fatal() {
        let dir = plugin_dir("unknown-fields");
        let m = read_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "warned-plugin");
        // 不赋语义：未知顶层字段不会落到 extensions 里，也不影响其它字段。
        assert!(m.extensions.is_empty(), "{:?}", m.extensions);
    }

    #[test]
    fn missing_component_dirs_are_ok() {
        let dir = plugin_dir("components-missing");
        assert_eq!(read_manifest(dir.path()).unwrap().name, "bare-plugin");
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

    #[test]
    fn bad_version_shapes_rejected() {
        let dir = plugin_dir("bad-version");
        let msg = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("field `version` must be a string"), "{msg}");
        assert!(msg.contains("number"), "{msg}");

        let cases = [
            (
                format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"demo"}}"#),
                "field `version` is required",
            ),
            (manifest_json("\"version\":\"\""), "must not be empty"),
            (manifest_json(r#""version":" ""#), "must not be empty"),
            (manifest_json(r#""version":["1.0.0"]"#), "must be a string"),
        ];
        for (json, expected) in cases {
            let msg = error_of(&json);
            assert!(msg.contains(expected), "{expected} not in: {msg}");
        }
    }

    #[test]
    fn semver_versions_accepted_and_non_semver_only_warns() {
        // §5.4：非 SemVer 不致命（discovery 层就依赖 `2-project` 这类版本）。
        for version in [
            "1.0.0",
            "0.1.0",
            "1.2.3-alpha.1",
            "1.2.3+build.5",
            "1.0.0-rc.1+20260905",
            "2-project",
            "latest",
        ] {
            let json = format!(
                r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"demo","version":"{version}"}}"#
            );
            assert_eq!(
                load_json(&json).unwrap().version,
                version,
                "{version} should load"
            );
        }
        for version in ["01.0.0", "1.0", "1.0.0.0", "1.0.0-"] {
            let json = format!(
                r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"demo","version":"{version}"}}"#
            );
            assert!(
                load_json(&json).is_ok(),
                "{version} must load with a warning"
            );
        }
    }

    #[test]
    fn bad_field_types_rejected_with_field_names() {
        let cases = [
            (manifest_json(r#""description":5"#), "field `description`"),
            (manifest_json(r#""homepage":true"#), "field `homepage`"),
            (manifest_json(r#""repository":[]"#), "field `repository`"),
            (
                manifest_json(r#""license":{"spdx":"MIT"}"#),
                "field `license`",
            ),
            (manifest_json(r#""keywords":"tag""#), "field `keywords`"),
            (
                manifest_json(r#""keywords":["ok",42]"#),
                "field `keywords[1]`",
            ),
            (manifest_json(r#""author":42"#), "field `author`"),
            (
                r#"{"$schema":123,"name":"demo","version":"1.0.0"}"#.to_string(),
                "field `$schema` must be a string",
            ),
            (
                format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":7,"version":"1.0.0"}}"#),
                "field `name` must be a string",
            ),
            (
                format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","version":"1.0.0"}}"#),
                "field `name` is required",
            ),
            (
                r#"["not","an","object"]"#.to_string(),
                "must contain a JSON object",
            ),
        ];
        for (json, expected) in cases {
            let msg = error_of(&json);
            assert!(msg.contains(expected), "{expected} not in: {msg}");
        }
    }

    #[test]
    fn explicit_null_fields_are_treated_as_absent() {
        let m = load_json(&manifest_json(
            r#""description":null,"author":null,"homepage":null,"repository":null,"license":null,"keywords":null,"extensions":null,"x":null"#,
        ))
        .unwrap();
        assert!(m.description.is_none() && m.author.is_none());
        assert!(m.keywords.is_empty() && m.extensions.is_empty());

        assert!(error_of(&manifest_json(r#""name":null"#)).contains("field `name` is required"));
        assert!(
            error_of(&manifest_json(r#""version":null"#)).contains("field `version` is required")
        );
    }

    #[test]
    fn author_object_shape_is_closed() {
        let msg = error_of(&manifest_json(r#""author":{"name":"A","mastodon":"@a"}"#));
        assert!(msg.contains("field `author`"), "{msg}");
        assert!(msg.contains("mastodon"), "{msg}");

        let msg = error_of(&manifest_json(r#""author":{"name":42}"#));
        assert!(msg.contains("field `author.name`"), "{msg}");

        let msg = error_of(&manifest_json(r#""author":{"email":"a@b.test"}"#));
        assert!(msg.contains("field `author`"), "{msg}");
        assert!(msg.contains("must carry `name`"), "{msg}");

        // 字符串 author，以及非 URL / 非邮箱的值都合法（§5.4 MUST NOT 拒绝）。
        assert!(load_json(&manifest_json(r#""author":"Some One""#)).is_ok());
        assert!(load_json(&manifest_json(
            r#""author":{"name":"A","email":"not-an-email","url":"not a url"}"#
        ))
        .is_ok());
    }

    #[test]
    fn extensions_field_shape_rules() {
        // 整体非对象：报告并忽略（§8.1 非致命）。
        let dir = plugin_dir("extensions-ignored");
        let m = read_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "ext-ignored-plugin");
        assert!(m.extensions.is_empty(), "{:?}", m.extensions);
        assert!(load_json(&manifest_json(r#""extensions":["a"]"#))
            .unwrap()
            .extensions
            .is_empty());

        // 命名空间值必须是对象，键必须是反域名。
        let msg = error_of(&manifest_json(r#""extensions":{"com.example.tools":true}"#));
        assert!(msg.contains("extensions.com.example.tools"), "{msg}");
        assert!(msg.contains("must be an object"), "{msg}");

        let msg = error_of(&manifest_json(r#""extensions":{"badnamespace":{}}"#));
        assert!(msg.contains("reverse-domain namespace"), "{msg}");
        assert!(msg.contains("badnamespace"), "{msg}");

        // 未实现命名空间：只看形状，不验证内容（§11.1）。
        let m = load_json(&manifest_json(
            r#""extensions":{"com.acme.app":{"whatever":[1,2,{"x":null}],"deep":{"a":true}}}"#,
        ))
        .unwrap();
        assert!(m.extensions.contains_key("com.acme.app"));
    }

    #[test]
    fn instagent_namespace_flags_are_typed() {
        // 自己的命名空间：未知标志前向兼容，minKernel 有类型要求。
        let m = load_json(&manifest_json(
            r#""extensions":{"dev.instagent":{"minKernel":"0.1","futureFlag":true}}"#,
        ))
        .unwrap();
        assert!(m.extensions.contains_key("dev.instagent"));

        for bad in [r#"1"#, r#""latest""#, r#"{"a":1}"#, r#"null"#] {
            let msg = error_of(&manifest_json(&format!(
                r#""extensions":{{"dev.instagent":{{"minKernel":{bad}}}}}"#
            )));
            assert!(msg.contains("minKernel"), "{bad}: {msg}");
            assert!(msg.contains("dotted version"), "{bad}: {msg}");
        }
    }

    #[test]
    fn errors_carry_plugin_json_path_and_plugin_name() {
        let dir = plugin_dir("bad-version");
        let msg = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("plugin.json"), "{msg}");
        assert!(msg.contains("versioned-bad"), "{msg}");

        let msg = error_of(&manifest_json(r#""description":5"#));
        assert!(msg.contains("plugin.json"), "{msg}");
        assert!(msg.contains("plugin `demo`"), "{msg}");
    }

    #[test]
    fn oversized_manifest_is_rejected_before_parse() {
        let padding = "a".repeat(MAX_MANIFEST_BYTES as usize);
        let json = format!(
            r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"demo","version":"1.0.0","description":"{padding}"}}"#
        );
        let dir = dir_with_manifest(&json);
        let msg = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("byte limit"), "{msg}");
        assert!(msg.contains("plugin.json"), "{msg}");
        assert!(
            msg.len() < 2048,
            "错误消息不能被坏文件内容放大：{} bytes",
            msg.len()
        );
    }

    #[test]
    fn parse_error_summary_is_bounded() {
        let json = format!(
            r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","a":{}"#,
            "b".repeat(200_000)
        );
        let dir = dir_with_manifest(&json);
        let msg = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("Failed to parse"), "{msg}");
        assert!(msg.contains('…'), "输入摘要必须截断: {msg}");
        assert!(
            !msg.contains(&"b".repeat(500)),
            "不得回显无界原始 JSON: {msg}"
        );
        assert!(msg.len() < 512, "错误消息过长：{} bytes", msg.len());
    }
}
