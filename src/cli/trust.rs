//! 信任确认（第三版 §2.10）：插件里只要有会执行命令的东西
//! （mcp.json、hooks、command tools、proxy provider），第一次启用时列出
//! 全部命令让用户确认，结果记到 settings 的 `trustedPlugins`；`--yes` 跳过。
//! 未信任的插件：加载但拒绝拉起任何命令（调用方按 [`plugin_surfaces`] 门控）。

use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use serde_json::Value;

use instagent::hooks::Hooks;
use instagent::plugin::install::plugin_data_dir;
use instagent::plugin::mcp_config::load_servers;
use instagent::plugin::mcp_config::McpServerType;
use instagent::plugin::Plugin;
use instagent::plugin::PluginSet;
use instagent::settings::Settings;
use instagent::tools::CommandTools;

/// 一个会执行命令的组件面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    /// "mcp" | "hook" | "tool" | "provider"
    pub kind: &'static str,
    /// 将执行的命令（或 http 连接目标），展示给用户。
    pub command: String,
}

/// 收集一个插件全部会执行命令的组件（展示用；不做展开失败的中断）。
pub fn plugin_surfaces(plugin: &Plugin) -> instagent::Result<Vec<Surface>> {
    let mut surfaces = Vec::new();
    let data = plugin_data_dir(&plugin.manifest.name)?;

    for server in load_servers(plugin, &data)? {
        let command = match server.r#type {
            McpServerType::Stdio => format!(
                "{} {}",
                server.command.clone().unwrap_or_default(),
                server.args.join(" ")
            )
            .trim()
            .to_string(),
            McpServerType::StreamableHttp | McpServerType::Sse => {
                server.url.clone().unwrap_or_else(|| "<missing url>".into())
            }
        };
        surfaces.push(Surface {
            kind: "mcp",
            command: format!("[{}] {command}", server.name),
        });
    }

    let single = PluginSet {
        plugins: vec![plugin.clone()],
        skipped: Vec::new(),
    };
    for entry in Hooks::load(&single)?.entries {
        for hook in entry.group.hooks {
            surfaces.push(Surface {
                kind: "hook",
                command: hook.command,
            });
        }
    }
    for instance in CommandTools::load(&single)? {
        for tool in instance.tools {
            surfaces.push(Surface {
                kind: "tool",
                command: format!("[{kind}] {cmd}", kind = tool.name, cmd = tool.command),
            });
        }
    }

    let providers = plugin
        .root
        .join(instagent::plugin::NAMESPACE)
        .join("providers");
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&providers) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    files.sort();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("engine").and_then(Value::as_str) != Some("proxy") {
            continue;
        }
        let command = value
            .get("proxy")
            .map(|proxy| {
                let cmd = proxy
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing command>");
                let args = proxy
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                format!("{cmd} {args}").trim().to_string()
            })
            .unwrap_or_default();
        surfaces.push(Surface {
            kind: "provider",
            command: format!("[{}] {command}", plugin.manifest.name),
        });
    }
    Ok(surfaces)
}

/// 用户层 settings 里的 `trustedPlugins`。
pub fn user_trusted() -> instagent::Result<Vec<String>> {
    Ok(Settings::load_user()?.trusted_plugins)
}

/// 记入用户层 settings 的 `trustedPlugins`（只动信任字段，不合并其他层）。
pub fn persist_trust(name: &str) -> instagent::Result<()> {
    let mut settings = Settings::load_user()?;
    if !settings.trusted_plugins.iter().any(|t| t == name) {
        settings.trusted_plugins.push(name.to_string());
    }
    settings
        .save_user()
        .with_context(|| format!("persist trustedPlugins for `{name}`"))
}

/// 首次启用确认：列出全部命令，读一行 y/n。EOF / 非 y 一律视为不信任。
pub fn confirm_plugin(
    name: &str,
    surfaces: &[Surface],
    reader: &mut dyn BufRead,
    out: &mut dyn Write,
) -> instagent::Result<bool> {
    writeln!(out, "plugin `{name}` wants to run the following commands:")?;
    for surface in surfaces {
        writeln!(out, "  [{}] {}", surface.kind, surface.command)?;
    }
    write!(out, "trust plugin `{name}`? [y]es / [n]o: ")?;
    out.flush()?;
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return Ok(false),
        Ok(_) => {}
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// 确认 + 可选持久化；返回是否信任。`surfaces` 为空视为天然信任。
pub fn ensure_trusted(
    plugin: &Plugin,
    surfaces: &[Surface],
    trusted: &mut Vec<String>,
    assume_yes: bool,
    reader: &mut dyn BufRead,
    out: &mut dyn Write,
) -> instagent::Result<bool> {
    let name = &plugin.manifest.name;
    if surfaces.is_empty() || trusted.iter().any(|t| t == name) {
        return Ok(true);
    }
    let granted = if assume_yes {
        true
    } else {
        confirm_plugin(name, surfaces, reader, out)?
    };
    if granted {
        persist_trust(name)?;
        trusted.push(name.clone());
    }
    Ok(granted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::fixtures::{self, Env};
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn surfaces_cover_all_four_component_kinds() {
        let env = Env::new();
        let plugin_dir = env.user_plugin("allkinds");
        fixtures::add_exec_surfaces(&plugin_dir);
        fixtures::add_mcp_json(
            &plugin_dir,
            json!({
                "local": { "type": "stdio", "command": "./bin/server", "args": ["--x"] },
                "remote": { "type": "streamable-http", "url": "https://example.com/mcp" },
            }),
        );
        fixtures::add_provider(&plugin_dir, fixtures::proxy_provider());

        let plugin = Plugin {
            manifest: instagent::plugin::manifest::read_manifest(&plugin_dir).unwrap(),
            root: plugin_dir,
            source: instagent::plugin::PluginSource::User,
        };
        let surfaces = plugin_surfaces(&plugin).unwrap();
        let kinds: Vec<&str> = surfaces.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, ["mcp", "mcp", "hook", "tool", "provider"]);
        let rendered = surfaces
            .iter()
            .map(|s| format!("{}:{}", s.kind, s.command))
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("mcp:[local] ./bin/server --x")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("mcp:[remote] https://example.com/mcp")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("hook:") && r.contains("guard.sh")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|r| r.contains("tool:[weather]")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("./serve.sh --port ${PORT}")),
            "{rendered:?}"
        );
    }

    #[test]
    fn confirm_yes_persists_trust_and_no_does_not() {
        let env = Env::new();
        let plugin_dir = env.user_plugin("trustme");
        fixtures::add_exec_surfaces(&plugin_dir);
        let plugin = Plugin {
            manifest: instagent::plugin::manifest::read_manifest(&plugin_dir).unwrap(),
            root: plugin_dir,
            source: instagent::plugin::PluginSource::User,
        };
        let surfaces = plugin_surfaces(&plugin).unwrap();
        assert!(!surfaces.is_empty());

        let mut trusted = Vec::new();
        let mut reader = Cursor::new(b"maybe\ny\n".to_vec());
        let mut out = Vec::new();
        // 第一行非 y → 不信任；再来一次输入 y → 信任并落盘。
        assert!(!ensure_trusted(
            &plugin,
            &surfaces,
            &mut trusted,
            false,
            &mut reader,
            &mut out
        )
        .unwrap());
        assert!(user_trusted().unwrap().is_empty());
        assert!(ensure_trusted(
            &plugin,
            &surfaces,
            &mut trusted,
            false,
            &mut reader,
            &mut out
        )
        .unwrap());
        assert_eq!(trusted, vec!["trustme".to_string()]);
        assert_eq!(user_trusted().unwrap(), vec!["trustme".to_string()]);
        let prompt = String::from_utf8(out).unwrap();
        assert!(
            prompt.contains("wants to run the following commands"),
            "{prompt}"
        );
        assert!(prompt.contains("[hook]"), "{prompt}");

        // 已信任后不再提问；空 surfaces（如无命令插件）天然信任。
        let mut reader2 = Cursor::new(Vec::new());
        let mut out2 = Vec::new();
        assert!(ensure_trusted(
            &plugin,
            &surfaces,
            &mut trusted,
            false,
            &mut reader2,
            &mut out2
        )
        .unwrap());
        assert!(out2.is_empty());
        let quiet = env.user_plugin("silent");
        let quiet = Plugin {
            manifest: instagent::plugin::manifest::read_manifest(&quiet).unwrap(),
            root: quiet,
            source: instagent::plugin::PluginSource::User,
        };
        assert!(ensure_trusted(&quiet, &[], &mut trusted, false, &mut reader2, &mut out2).unwrap());
    }
}
