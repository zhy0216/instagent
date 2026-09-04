//! 插件 `mcp.json`（规范格式）解析 + 变量展开 + `.mcp.json` 兼容（第三版 §2.3）。
//!
//! - `mcpServers` 每项 `type` 三选一：`stdio` / `streamable-http` / `sse`。
//!   v1 只实现前两个的执行结构；`sse` 照常解析并标记 [`McpServerType::Sse`]，
//!   由上层跳过（跳过时给可读提示）。
//! - `command` 必须是单个可执行名或 `./` 开头的插件相对路径（不得含 `..`
//!   逃逸插件根），不做变量展开。
//! - 只展开 `${PLUGIN_ROOT}` 与 `${PLUGIN_DATA}`：单次、非递归，只作用于
//!   `args` 元素、`env` 值、`cwd`；其他 `${...}` 原样保留；`env` 里禁止定义
//!   这两个保留名。`${PLUGIN_DATA}` 的目录路径由调用方传入（创建归 `07` 的
//!   `install::plugin_data_dir`）。
//! - `url` 与 `headers` 不展开、不承载凭据（远程鉴权 v1 不做）。
//! - 插件根没有 `mcp.json` 时回退读 `.mcp.json`（goose / Claude Code 草案
//!   格式，无 `type` 字段按 `stdio` 处理）；两者都没有则返回空列表。
//! - 读取有大小上限（[`MAX_MCP_CONFIG_BYTES`]），错误诊断带来源文件路径；
//!   回显进错误里的用户输入（command 等）做截断，防坏配置放大日志。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;
use serde::Serialize;

use crate::plugin::Plugin;

/// `mcp.json` / `.mcp.json` 单次读取的硬上限（1 MiB）：超限直接拒绝解析，
/// 不让坏文件把整坨内容读进内存或灌进错误日志。
pub const MAX_MCP_CONFIG_BYTES: u64 = 1024 * 1024;

/// 回显进错误消息里的用户输入的最大字符数，超出截断加省略号。
const MAX_ECHO_CHARS: usize = 120;

fn brief(value: &str) -> String {
    let mut out: String = value.chars().take(MAX_ECHO_CHARS).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerType {
    Stdio,
    StreamableHttp,
    /// v1 不实现，跳过时给可读提示。
    Sse,
}

/// 单个 MCP server 的运行时配置（已完成变量展开）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 插件内的 server 名（工具前缀 `<server>__<tool>` 用）。
    pub name: String,
    pub r#type: McpServerType,
    /// stdio：单个可执行名或 `./` 开头的插件相对路径，不做变量展开。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// streamable-http。`headers` 不承载凭据（规范规定，远程鉴权 v1 不做）。
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, RawServer>,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    #[serde(rename = "type", default)]
    server_type: Option<McpServerType>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// 先按任意 JSON 收下，逐条校验字符串形状，错误能指到具体 header 名。
    #[serde(default)]
    headers: BTreeMap<String, serde_json::Value>,
}

/// `headers` 必须是 string → string；坏条目的错误带来源路径、server 与 header 名。
fn validate_headers(
    path: &Path,
    name: &str,
    headers: &BTreeMap<String, serde_json::Value>,
) -> crate::Result<BTreeMap<String, String>> {
    headers
        .iter()
        .map(|(key, value)| match value.as_str() {
            Some(v) => Ok((key.clone(), v.to_string())),
            None => bail!(
                "{}: mcp server `{name}` header `{}` must be a string value, got {}",
                path.display(),
                brief(key),
                brief(&value.to_string())
            ),
        })
        .collect()
}

/// 读插件根下的 `mcp.json`（无则回退 `.mcp.json`，再无则空列表）并展开变量。
///
/// `plugin_data` 是该插件的 `PLUGIN_DATA` 目录（`07` 的
/// `install::plugin_data_dir`），本函数只展开、不创建。
pub fn load_servers(plugin: &Plugin, plugin_data: &Path) -> crate::Result<Vec<McpServerConfig>> {
    let (path, draft) = match select_config_file(&plugin.root)? {
        None => return Ok(Vec::new()),
        Some(pair) => pair,
    };
    let text = read_bounded(&path)?;
    let file: RawFile = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    file.mcp_servers
        .into_iter()
        .map(|(name, raw)| expand_server(name, raw, draft, &path, &plugin.root, plugin_data))
        .collect()
}

/// 带硬上限的配置文件读取：先查 metadata 再读，超限错误指出路径与上限。
fn read_bounded(path: &Path) -> crate::Result<String> {
    let size = fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    if size > MAX_MCP_CONFIG_BYTES {
        bail!(
            "{}: mcp config is {size} bytes, over the {MAX_MCP_CONFIG_BYTES} byte limit",
            path.display()
        );
    }
    fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))
}

/// 返回 (配置文件路径, 是否草案 `.mcp.json`)；两者都不存在时 `None`。
fn select_config_file(root: &Path) -> crate::Result<Option<(PathBuf, bool)>> {
    let normative = root.join("mcp.json");
    if normative.is_file() {
        return Ok(Some((normative, false)));
    }
    let draft = root.join(".mcp.json");
    if draft.is_file() {
        return Ok(Some((draft, true)));
    }
    Ok(None)
}

fn expand_server(
    name: String,
    raw: RawServer,
    draft: bool,
    path: &Path,
    plugin_root: &Path,
    plugin_data: &Path,
) -> crate::Result<McpServerConfig> {
    let r#type = match (raw.server_type, draft) {
        (Some(t), _) => t,
        // 草案格式没有 `type` 字段，按 stdio 处理；规范格式必须显式声明。
        (None, true) => McpServerType::Stdio,
        (None, false) => bail!(
            "{}: mcp server `{name}` is missing required field `type` \
             (stdio / streamable-http / sse)",
            path.display()
        ),
    };

    for key in raw.env.keys() {
        if key == "PLUGIN_ROOT" || key == "PLUGIN_DATA" {
            bail!(
                "{}: mcp server `{name}` must not define reserved variable `{}` in `env`",
                path.display(),
                brief(key)
            );
        }
    }

    match r#type {
        McpServerType::Stdio => {
            let command = raw.command.clone().unwrap_or_default();
            if raw.command.is_none() {
                bail!(
                    "{}: mcp server `{name}` of type stdio requires `command`",
                    path.display()
                );
            }
            validate_command(path, &name, &command)?;
        }
        McpServerType::StreamableHttp => {
            if raw.url.is_none() {
                bail!(
                    "{}: mcp server `{name}` of type streamable-http requires `url`",
                    path.display()
                );
            }
        }
        // v1 不实现：只做结构解析，由上层按 Sse 标记跳过。
        McpServerType::Sse => {}
    }

    let headers = validate_headers(path, &name, &raw.headers)?;
    let expand = |value: &str| expand_vars(value, plugin_root, plugin_data);
    Ok(McpServerConfig {
        name,
        r#type,
        command: raw.command,
        args: raw.args.iter().map(|a| expand(a)).collect(),
        env: raw
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand(v)))
            .collect(),
        cwd: raw.cwd.as_deref().map(|c| PathBuf::from(expand(c))),
        // `url` / `headers` 不参与展开。
        url: raw.url,
        headers,
    })
}

/// 单个可执行名，或 `./` 开头、不逃逸插件根的相对路径；不做变量展开。
fn validate_command(path: &Path, name: &str, command: &str) -> crate::Result<()> {
    if command.is_empty() {
        bail!(
            "{}: mcp server `{name}` has an empty `command`",
            path.display()
        );
    }
    if command.contains("${") {
        bail!(
            "{}: mcp server `{name}` `command` must not contain variables (no expansion is \
             performed): `{}`",
            path.display(),
            brief(command)
        );
    }
    if command.chars().any(char::is_whitespace) {
        bail!(
            "{}: mcp server `{name}` `command` must be a single executable name or a `./`-prefixed \
             plugin-relative path: `{}`",
            path.display(),
            brief(command)
        );
    }
    if command.starts_with("./") {
        if command.split('/').any(|seg| seg == "..") {
            bail!(
                "{}: mcp server `{name}` `command` must not escape the plugin root: `{}`",
                path.display(),
                brief(command)
            );
        }
        return Ok(());
    }
    if command.contains('/') {
        bail!(
            "{}: mcp server `{name}` relative `command` paths must start with `./`: `{}`",
            path.display(),
            brief(command)
        );
    }
    Ok(())
}

/// 单次、非递归地展开 `${PLUGIN_ROOT}` / `${PLUGIN_DATA}`，其余 `${...}` 原样保留。
fn expand_vars(value: &str, plugin_root: &Path, plugin_data: &Path) -> String {
    const ROOT: &str = "${PLUGIN_ROOT}";
    const DATA: &str = "${PLUGIN_DATA}";
    let root = plugin_root.display().to_string();
    let data = plugin_data.display().to_string();
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix(ROOT) {
            out.push_str(&root);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(DATA) {
            out.push_str(&data);
            rest = tail;
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PluginManifest;
    use crate::plugin::PluginSource;
    use std::path::Path;

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mcp_config")
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

    /// 把 fixture 目录整体拷进 tempdir 当作插件根。
    fn plugin_dir(fixture: &str) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        copy_dir(&fixture_path(fixture), tmp.path());
        tmp
    }

    /// 直接把 mcp.json 内容写入空的 tempdir。
    fn dir_with_mcp_json(json: &str) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("mcp.json"), json).unwrap();
        tmp
    }

    fn plugin_at(root: &Path) -> Plugin {
        let manifest: PluginManifest =
            serde_json::from_str(r#"{"name": "demo", "version": "0.1.0"}"#).unwrap();
        Plugin {
            manifest,
            root: root.to_path_buf(),
            source: PluginSource::User,
        }
    }

    fn load(root: &Path, data: &Path) -> crate::Result<Vec<McpServerConfig>> {
        load_servers(&plugin_at(root), data)
    }

    #[test]
    fn expands_plugin_root_and_data_only_where_allowed() {
        let dir = plugin_dir("expand");
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        assert_eq!(servers.len(), 2);

        let everything = &servers[0];
        assert_eq!(everything.name, "everything");
        assert_eq!(everything.r#type, McpServerType::Stdio);
        assert_eq!(everything.command.as_deref(), Some("npx"));
        let root = dir.path().display().to_string();
        let data_str = data.path().display().to_string();
        assert_eq!(
            everything.args,
            vec![
                "-y".to_string(),
                format!("{root}/server.js"),
                format!("{data_str}/cache"),
                "${HOME}/keep-me".to_string(),
            ]
        );
        assert_eq!(everything.env["ROOT"], root);
        assert_eq!(everything.env["DATA_DIR"], format!("{data_str}/d"));
        assert_eq!(everything.env["UNKNOWN"], "${NOPE}x");
        assert_eq!(everything.cwd, Some(PathBuf::from(dir.path())));

        let remote = &servers[1];
        assert_eq!(remote.r#type, McpServerType::StreamableHttp);
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(remote.command, None);
        assert!(remote.args.is_empty() && remote.env.is_empty() && remote.cwd.is_none());
    }

    #[test]
    fn headers_and_url_are_not_expanded() {
        let dir = dir_with_mcp_json(
            r#"{"mcpServers": {"h": {"type": "streamable-http",
                "url": "https://example.com/${PLUGIN_ROOT}",
                "headers": {"X-Root": "${PLUGIN_ROOT}", "X-Env": "${SOME_ENV}"}}}}"#,
        );
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        assert_eq!(
            servers[0].url.as_deref(),
            Some("https://example.com/${PLUGIN_ROOT}")
        );
        assert_eq!(servers[0].headers["X-Root"], "${PLUGIN_ROOT}");
        assert_eq!(servers[0].headers["X-Env"], "${SOME_ENV}");
    }

    #[test]
    fn expansion_is_single_pass_all_occurrences() {
        let dir = dir_with_mcp_json(
            r#"{"mcpServers": {"s": {"type": "stdio", "command": "srv",
                "args": ["${PLUGIN_ROOT}${PLUGIN_ROOT}", "$${PLUGIN_ROOT}"]}}}"#,
        );
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        let root = dir.path().display().to_string();
        assert_eq!(servers[0].args[0], format!("{root}{root}"));
        assert_eq!(servers[0].args[1], format!("${root}"));
    }

    #[test]
    fn sse_is_parsed_and_marked_unsupported() {
        let dir = plugin_dir("sse");
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "legacy");
        assert_eq!(servers[0].r#type, McpServerType::Sse);
        assert_eq!(servers[0].url.as_deref(), Some("https://example.com/sse"));
        assert_eq!(servers[1].r#type, McpServerType::Stdio);
    }

    #[test]
    fn env_defining_reserved_names_errors() {
        let dir = plugin_dir("reserved-env");
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("reserved variable"), "{msg}");
        assert!(msg.contains("PLUGIN_ROOT"), "{msg}");

        let dir = dir_with_mcp_json(
            r#"{"mcpServers": {"x": {"type": "stdio", "command": "srv",
                "env": {"PLUGIN_DATA": "/tmp"}}}}"#,
        );
        let err = load(dir.path(), data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("reserved variable"), "{msg}");
        assert!(msg.contains("PLUGIN_DATA"), "{msg}");
    }

    #[test]
    fn draft_mcp_json_fallback_defaults_to_stdio() {
        let dir = plugin_dir("draft");
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.r#type, McpServerType::Stdio);
        assert_eq!(s.command.as_deref(), Some("uvx"));
        assert_eq!(s.args, vec!["mcp-drafty".to_string()]);
        assert_eq!(s.env["A"], format!("{}/a", dir.path().display()));
    }

    #[test]
    fn normative_mcp_json_wins_over_draft() {
        let dir = plugin_dir("both");
        let data = tempfile::TempDir::new().unwrap();
        let servers = load(dir.path(), data.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "normative");
    }

    #[test]
    fn missing_both_files_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        assert!(load(tmp.path(), data.path()).unwrap().is_empty());
    }

    #[test]
    fn stdio_requires_command() {
        let dir = dir_with_mcp_json(r#"{"mcpServers": {"s": {"type": "stdio"}}}"#);
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        assert!(err.to_string().contains("requires `command`"), "{err}");
    }

    #[test]
    fn http_requires_url() {
        let dir = dir_with_mcp_json(r#"{"mcpServers": {"s": {"type": "streamable-http"}}}"#);
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        assert!(err.to_string().contains("requires `url`"), "{err}");
    }

    #[test]
    fn normative_requires_type() {
        let dir = dir_with_mcp_json(r#"{"mcpServers": {"s": {"command": "srv"}}}"#);
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        assert!(
            err.to_string().contains("missing required field `type`"),
            "{err}"
        );
    }

    #[test]
    fn invalid_commands_rejected() {
        let data = tempfile::TempDir::new().unwrap();
        let cases = [
            ("npx -y pkg", "single executable"),
            ("${PLUGIN_ROOT}/bin/srv", "must not contain variables"),
            ("bin/srv", "must start with `./`"),
            ("/usr/local/bin/srv", "must start with `./`"),
            ("./bin/../..", "must not escape"),
            ("", "empty"),
        ];
        for (command, expect) in cases {
            let json = format!(
                r#"{{"mcpServers": {{"s": {{"type": "stdio", "command": "{command}"}}}}}}"#,
            );
            let dir = dir_with_mcp_json(&json);
            let err = load(dir.path(), data.path())
                .err()
                .unwrap_or_else(|| panic!("`{command}` should be rejected"));
            let msg = err.to_string();
            assert!(msg.contains(expect), "`{command}`: {msg}");
        }
    }

    #[test]
    fn valid_command_forms_accepted() {
        let data = tempfile::TempDir::new().unwrap();
        for command in ["npx", "./bin/srv", "./a/b-c.d_x"] {
            let json = format!(
                r#"{{"mcpServers": {{"s": {{"type": "stdio", "command": "{command}"}}}}}}"#
            );
            let dir = dir_with_mcp_json(&json);
            let servers =
                load(dir.path(), data.path()).unwrap_or_else(|e| panic!("{command}: {e}"));
            assert_eq!(servers[0].command.as_deref(), Some(command));
        }
    }

    #[test]
    fn unparsable_json_errors() {
        let dir = dir_with_mcp_json("{ not json");
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"), "{err}");
        assert!(err.to_string().contains("mcp.json"), "{err}");
    }

    #[test]
    fn oversized_config_is_rejected_before_parse() {
        let padding = "a".repeat(MAX_MCP_CONFIG_BYTES as usize);
        let json = format!(
            r#"{{"mcpServers": {{"s": {{"type": "stdio", "command": "srv", "args": ["{padding}]}}}}}}"#
        );
        let dir = dir_with_mcp_json(&json);
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("byte limit"), "{msg}");
        assert!(msg.contains("mcp.json"), "{msg}");
        assert!(
            msg.len() < 2048,
            "错误消息不能被坏文件内容放大：{} bytes",
            msg.len()
        );
    }

    #[test]
    fn invalid_headers_shape_errors_with_source_path() {
        let dir = dir_with_mcp_json(
            r#"{"mcpServers": {"h": {"type": "streamable-http", "url": "https://x.invalid/mcp",
                "headers": {"X-Ok": "v", "X-Bad": 123}}}}"#,
        );
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mcp.json"), "{msg}");
        assert!(msg.contains("header `X-Bad`"), "{msg}");
        assert!(msg.contains("`h`"), "{msg}");
    }

    #[test]
    fn error_echoes_of_user_input_are_bounded() {
        let long_command = format!("{} ", "x".repeat(500));
        let dir = dir_with_mcp_json(&format!(
            r#"{{"mcpServers": {{"s": {{"type": "stdio", "command": "{long_command}"}}}}}}"#
        ));
        let data = tempfile::TempDir::new().unwrap();
        let err = load(dir.path(), data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("single executable"), "{msg}");
        assert!(msg.contains('…'), "command 回显必须截断: {msg}");
        assert!(msg.len() < 400, "错误消息过长：{} bytes", msg.len());
    }
}
