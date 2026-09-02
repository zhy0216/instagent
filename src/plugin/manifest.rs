//! `plugin.json`（Agent Plugins v1.0.0）解析与校验（第三版 §2.1、§2.2）。
//!
//! TODO(04)：填校验逻辑（name 规则、`$schema` 字符串匹配、未知字段收集为
//! warning、`extensions["dev.instagent"]` 的 `minKernel` 解析）。
//! 参考 goose `plugins/formats/open_plugins.rs`（只读）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

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

/// 校验产物：manifest + 非致命 warning（未知字段之类）。
#[derive(Debug, Clone)]
pub struct Validated {
    pub manifest: PluginManifest,
    pub warnings: Vec<String>,
}

/// 读 `<dir>/plugin.json` 并校验（规范 v1.0.0 位置，非 goose 草案路径）。TODO(04)
pub fn read_manifest(_dir: &Path) -> crate::Result<Validated> {
    todo!("TODO(04)")
}

/// name：1~64 字符，小写字母数字与 `-` `.`，首尾字母数字，不含 `--` `..`。TODO(04)
pub fn validate_plugin_name(_name: &str) -> crate::Result<()> {
    todo!("TODO(04)")
}

/// 插件内组件命名 `<plugin>:<skill>`（goose namespaced_component_name）。TODO(04)
pub fn namespaced_component_name(_plugin: &str, _component: &str) -> String {
    todo!("TODO(04)")
}
