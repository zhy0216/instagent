//! 插件斜杠命令（第三版 §2.8）：`dev.instagent/commands/*.md`，Claude Code
//! 约定。frontmatter 取 `description` / `argument-hint`，正文 `$ARGUMENTS`
//! 展开；无占位符且带参数时参数追加到末尾（Claude Code 行为）。

use std::path::PathBuf;

use serde::Deserialize;

use crate::plugin::PluginSet;
use crate::plugin::NAMESPACE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// 命令名 = 文件名（不含扩展名）。
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// 正文模板，`$ARGUMENTS` 展开。
    pub template: String,
}

#[derive(Debug, Default, Deserialize)]
struct CommandFrontmatter {
    #[serde(default, deserialize_with = "loose_optional_string")]
    description: Option<String>,
    #[serde(
        default,
        rename = "argument-hint",
        deserialize_with = "loose_optional_string"
    )]
    argument_hint: Option<String>,
}

/// 宽容取值：`argument-hint: [focus]` 在 YAML 里是 flow 序列，按 Claude 约定
/// 回读成字面量 `[focus]`；字符串原样，标量转字符串，序列用 `, ` 拼接。
fn loose_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.map(|value| match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| item
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| item.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }))
}

/// 发现启用插件的 `dev.instagent/commands/*.md`（插件名升序、文件名升序，
/// 同名命令先到先得）。解析失败的文件跳过（warn），不中断加载。
pub fn load_commands(plugins: &PluginSet) -> crate::Result<Vec<SlashCommand>> {
    let mut out = Vec::new();
    for plugin in plugins.iter() {
        let dir = plugin.root.join(NAMESPACE).join("commands");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for path in files {
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!("跳过 {}：读取失败 {err}", path.display());
                    continue;
                }
            };
            let (frontmatter, template) = match parse_command_file(&text) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!("跳过 {}：{err}", path.display());
                    continue;
                }
            };
            if out.iter().any(|cmd: &SlashCommand| cmd.name == name) {
                tracing::warn!("跳过 {}：命令 `/{name}` 重名，先到先得", path.display());
                continue;
            }
            out.push(SlashCommand {
                name,
                description: frontmatter.description,
                argument_hint: frontmatter.argument_hint,
                template,
            });
        }
    }
    Ok(out)
}

/// 拆分 `---` frontmatter（缺失容忍：整篇是正文）。返回（元信息，正文）。
fn parse_command_file(text: &str) -> crate::Result<(CommandFrontmatter, String)> {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((CommandFrontmatter::default(), text.trim().to_string()));
    };
    let mut fm_lines: Vec<&str> = Vec::new();
    let mut body_start: Option<usize> = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        fm_lines.push(line.trim_end_matches('\r'));
        offset += line.len();
    }
    let Some(body_start) = body_start else {
        anyhow::bail!("unterminated frontmatter: missing closing `---`");
    };
    let frontmatter: CommandFrontmatter = serde_yaml::from_str(&fm_lines.join("\n"))
        .map_err(|err| anyhow::anyhow!("invalid frontmatter YAML: {err}"))?;
    Ok((frontmatter, rest[body_start..].trim().to_string()))
}

/// 命令正文 + 用户参数 → 一条用户消息文本（`/review security` 的展开）。
pub fn expand(cmd: &SlashCommand, args: &str) -> String {
    let args = args.trim();
    if cmd.template.contains("$ARGUMENTS") {
        cmd.template.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        cmd.template.clone()
    } else {
        format!("{}\n\n{args}", cmd.template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use crate::plugin::{Plugin, PluginSource};
    use std::path::Path;

    fn plugin(root: &Path, name: &str) -> Plugin {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
        let validated = crate::plugin::manifest::read_manifest(root).unwrap();
        Plugin {
            manifest: validated.manifest,
            root: root.to_path_buf(),
            source: PluginSource::Extra,
        }
    }

    fn command_file(plugin_root: &Path, file: &str, content: &str) {
        let dir = plugin_root.join(NAMESPACE).join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), content).unwrap();
    }

    #[test]
    fn discovers_frontmatter_and_expands_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        command_file(
            &a,
            "review.md",
            "---\ndescription: Review the current diff\nargument-hint: [focus]\n---\n\
             Review `git diff` with focus on: $ARGUMENTS. Report findings as a list.\n",
        );
        let b = dir.path().join("b");
        command_file(
            &b,
            "deploy.md",
            "---\ndescription: Deploy it\n---\npush to prod\n",
        );
        let set = PluginSet {
            plugins: vec![plugin(&a, "a"), plugin(&b, "b")],
            skipped: Vec::new(),
        };

        let commands = load_commands(&set).unwrap();
        assert_eq!(commands.len(), 2);
        let review = commands.iter().find(|c| c.name == "review").unwrap();
        assert_eq!(
            review.description.as_deref(),
            Some("Review the current diff")
        );
        assert_eq!(review.argument_hint.as_deref(), Some("[focus]"));

        // `/review security` 展开成一条用户消息。
        let message = expand(review, "security");
        assert!(message.contains("focus on: security"), "{message}");
        assert!(!message.contains("$ARGUMENTS"));
        assert!(message.contains("Review `git diff`"));
        // 无参数时占位符展开为空。
        assert!(expand(review, "").contains("focus on: ."));
        // 无 $ARGUMENTS 占位符时参数追加到末尾（Claude Code 约定）。
        let deploy = commands.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(expand(deploy, "--force"), "push to prod\n\n--force");
        assert_eq!(expand(deploy, ""), "push to prod");
    }

    #[test]
    fn skips_invalid_and_duplicate_commands() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        command_file(&a, "broken.md", "---\ndescription: [unclosed\n---\nbody\n");
        command_file(&a, "dup.md", "first wins\n");
        command_file(&a, "not-frontmatter.md", "plain body without frontmatter\n");
        let b = dir.path().join("b");
        command_file(&b, "dup.md", "second loses\n");
        let set = PluginSet {
            plugins: vec![plugin(&a, "a"), plugin(&b, "b")],
            skipped: Vec::new(),
        };
        let commands = load_commands(&set).unwrap();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"dup"), "{names:?}");
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.name == "dup")
                .map(|c| c.template.as_str())
                .collect::<Vec<_>>(),
            vec!["first wins"],
            "同名先到先得"
        );
        let plain = commands
            .iter()
            .find(|c| c.name == "not-frontmatter")
            .unwrap();
        assert_eq!(plain.description, None, "无 frontmatter 容忍为正文");
        assert!(!names.contains(&"broken"), "坏 YAML 跳过：{names:?}");
    }

    #[test]
    fn plugins_without_commands_dir_are_fine() {
        let dir = tempfile::tempdir().unwrap();
        let set = PluginSet {
            plugins: vec![plugin(&dir.path().join("bare"), "bare")],
            skipped: Vec::new(),
        };
        assert!(load_commands(&set).unwrap().is_empty());
    }
}
