//! 无交互任务模板：保留 `dev.instagent/commands/*.md` 插件文件格式，
//! 通过 `run --command plugin:name --args ...` 提供任务。frontmatter 取
//! `description` / `argument-hint`，正文 `$ARGUMENTS` 展开；无占位符且
//! 带参数时参数追加到末尾。
//!
//! 目录枚举与正文读取都带可见诊断与硬上限（R12、S18）：坏 symlink、无权限、
//! 超长正文、解析失败、重名一律汇总成 [`SkippedCommand`]，不静默消失。

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::plugin::Plugin;
use crate::plugin::PluginSet;
use crate::plugin::NAMESPACE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTemplate {
    /// 完整模板名 = `plugin:文件名`（文件名不含扩展名），不提供裸名称别名。
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

/// 单个命令文件正文的硬上限（S18）：超限不静默截断，记 skipped 诊断。
const MAX_COMMAND_FILE_BYTES: u64 = 256 * 1024;

/// 一个被跳过的命令文件：路径 + 原因（原因内含插件名与来源，R12）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedCommand {
    pub path: PathBuf,
    pub reason: String,
}

/// 发现启用插件的 `dev.instagent/commands/*.md`（插件名升序、文件名升序，
/// 名称按 `plugin:name` 消歧）。坏目录、坏 symlink、无权限、超长正文、解析失败与重名
/// 都汇总成可见诊断（[`collect_commands`] 返回、[`load_commands`] 逐条 warn），
/// 不静默消失；解析失败不中断加载。
pub fn load_commands(plugins: &PluginSet) -> crate::Result<Vec<TaskTemplate>> {
    let (commands, skipped) = collect_commands(plugins);
    for skipped in &skipped {
        tracing::warn!("跳过 {}：{}", skipped.path.display(), skipped.reason);
    }
    Ok(commands)
}

/// [`load_commands`] 的诊断面：返回（命令列表，被跳过项）。装配层需要时可直接
/// 用它把 `skipped` 并入启动 notes。
pub fn collect_commands(plugins: &PluginSet) -> (Vec<TaskTemplate>, Vec<SkippedCommand>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for plugin in plugins.iter() {
        let dir = plugin.root.join(NAMESPACE).join("commands");
        let entries = match std::fs::read_dir(&dir) {
            // 插件不带 commands 目录属正常（第三版 §2.1）。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                skipped.push(diagnostic(
                    plugin,
                    &dir,
                    &format!("failed to read commands dir: {err}"),
                ));
                continue;
            }
            Ok(entries) => entries,
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                skipped.push(diagnostic(
                    plugin,
                    &dir,
                    "failed to read a dir entry of commands dir \
                     (permission denied, changed during scan, or unreadable directory?)",
                ));
                continue;
            };
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue; // 不匹配：非 Markdown，静默。
            }
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => files.push(path),
                // 指向目录 / 非文件的链接：不匹配，静默。
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => skipped.push(diagnostic(
                    plugin,
                    &path,
                    &format!("broken symlink or vanished file: {err}"),
                )),
                Err(err) => skipped.push(diagnostic(
                    plugin,
                    &path,
                    &format!("cannot stat command file: {err}"),
                )),
            }
        }
        files.sort();
        for path in files {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                skipped.push(diagnostic(plugin, &path, "template filename must be UTF-8"));
                continue;
            };
            let name = format!("{}:{stem}", plugin.manifest.name);
            let text = match read_bounded(&path) {
                Ok(text) => text,
                Err(reason) => {
                    skipped.push(diagnostic(plugin, &path, &reason));
                    continue;
                }
            };
            let (frontmatter, template) = match parse_command_file(&text) {
                Ok(parsed) => parsed,
                Err(err) => {
                    skipped.push(diagnostic(plugin, &path, &format!("{err:#}")));
                    continue;
                }
            };
            if out.iter().any(|cmd: &TaskTemplate| cmd.name == name) {
                skipped.push(diagnostic(
                    plugin,
                    &path,
                    &format!("task template `{name}` is duplicated"),
                ));
                continue;
            }
            out.push(TaskTemplate {
                name,
                description: frontmatter.description,
                argument_hint: frontmatter.argument_hint,
                template,
            });
        }
    }
    (out, skipped)
}

/// 一条命令文件诊断：`插件名 (来源) [绝对路径]: 原因`。
fn diagnostic(plugin: &Plugin, path: &Path, reason: &str) -> SkippedCommand {
    SkippedCommand {
        path: path.to_path_buf(),
        reason: format!(
            "plugin `{}` ({}: {}): {reason}",
            plugin.manifest.name,
            plugin.source.display_name(),
            crate::plugin::located(path)
        ),
    }
}

/// 有界读取命令正文：metadata 预检 + `take(MAX + 1)` 兜住预检与读取之间的
/// 增长竞态，超限给可诊断错误（不静默截断）。
fn read_bounded(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let size = std::fs::metadata(path).map_err(|err| format!("cannot stat command file: {err}"))?;
    if size.len() > MAX_COMMAND_FILE_BYTES {
        return Err(format!(
            "command file is {} bytes, over the {MAX_COMMAND_FILE_BYTES} byte limit",
            size.len()
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .map_err(|err| format!("failed to open command file: {err}"))?
        .take(MAX_COMMAND_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| format!("failed to read command file: {err}"))?;
    if text.len() as u64 > MAX_COMMAND_FILE_BYTES {
        return Err(format!(
            "command file grew past the {MAX_COMMAND_FILE_BYTES} byte limit while reading"
        ));
    }
    Ok(text)
}

/// 拆分 `---` frontmatter（缺失容忍：整篇是正文）。返回（元信息，正文）。
fn parse_command_file(text: &str) -> crate::Result<(CommandFrontmatter, String)> {
    crate::tools::skills::parse_frontmatter(text, true)
}

const ARGUMENTS: &str = "$ARGUMENTS";
const MAX_EXPANDED_TASK_BYTES: usize = 1024 * 1024;

/// 模板正文 + 调用参数 → 一条任务消息文本。
/// 保留原有无界调用方式；生产任务输入应使用 [`expand_bounded`]。
pub fn expand(cmd: &TaskTemplate, args: &str) -> String {
    let args = args.trim();
    if cmd.template.contains(ARGUMENTS) {
        cmd.template.replace(ARGUMENTS, args)
    } else if args.is_empty() {
        cmd.template.clone()
    } else {
        format!("{}\n\n{args}", cmd.template)
    }
}

/// 展开前校验 UTF-8 字节预算，结果至多 1 MiB；超限不分配展开文本或裁剪任务。
pub fn expand_bounded(cmd: &TaskTemplate, args: &str) -> crate::Result<String> {
    expand_with_limit(cmd, args, MAX_EXPANDED_TASK_BYTES)
}

fn expand_with_limit(cmd: &TaskTemplate, args: &str, max_bytes: usize) -> crate::Result<String> {
    let args = args.trim();
    let placeholders = cmd.template.matches(ARGUMENTS).count();
    let bytes = expanded_len(cmd.template.len(), placeholders, args.len()).ok_or_else(|| {
        anyhow::anyhow!("expanded task size overflow (input budget: {max_bytes} bytes)")
    })?;
    if bytes > max_bytes {
        anyhow::bail!(
            "expanded task would be {bytes} bytes, exceeding the {max_bytes} byte input budget"
        );
    }
    // The original expansion only runs after its complete output fits the budget.
    Ok(expand(cmd, args))
}

/// Pure byte accounting also lets tests exercise usize boundaries without huge strings.
fn expanded_len(template_bytes: usize, placeholders: usize, args_bytes: usize) -> Option<usize> {
    if placeholders == 0 {
        if args_bytes == 0 {
            Some(template_bytes)
        } else {
            template_bytes.checked_add(2)?.checked_add(args_bytes)
        }
    } else {
        let literal_bytes =
            template_bytes.checked_sub(placeholders.checked_mul(ARGUMENTS.len())?)?;
        literal_bytes.checked_add(placeholders.checked_mul(args_bytes)?)
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
        let manifest = crate::plugin::manifest::read_manifest(root).unwrap();
        Plugin {
            manifest,
            root: root.to_path_buf(),
            source: PluginSource::Extra,
        }
    }

    fn command_file(plugin_root: &Path, file: &str, content: &str) {
        let dir = plugin_root.join(NAMESPACE).join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), content).unwrap();
    }

    fn template(body: &str) -> TaskTemplate {
        TaskTemplate {
            name: "test:task".into(),
            description: None,
            argument_hint: None,
            template: body.into(),
        }
    }

    #[test]
    fn bounded_expansion_preserves_replacement_append_utf8_and_newlines() {
        for (body, args, expected) in [
            ("Review $ARGUMENTS.", " security ", "Review security."),
            ("$ARGUMENTS/$ARGUMENTS", "蓝莓🙂", "蓝莓🙂/蓝莓🙂"),
            ("$ARGUMENTS$ARGUMENTS", " x ", "xx"),
            ("[$ARGUMENTS]", "", "[]"),
            ("$ARGUMENTS\n$ARGUMENTS", " \t\r\n", "\n"),
            ("[$ARGUMENTS]", "\u{3000}é\u{2003}", "[é]"),
            ("[$ARGUMENTS]", "$ARGUMENTS", "[$ARGUMENTS]"),
            ("$$ARGUMENTS/$ARGUMENTSx/$ARGUMENT", "a", "$a/ax/$ARGUMENT"),
            (
                "first\r\n$ARGUMENTS\nlast\n",
                " a\r\nb \n",
                "first\r\na\r\nb\nlast\n",
            ),
            ("push to prod", " --force ", "push to prod\n\n--force"),
            ("正文\n", " 第一行\n第二行 ", "正文\n\n\n第一行\n第二行"),
            ("正文\r\n", " \t", "正文\r\n"),
            ("", "参数", "\n\n参数"),
            ("", " \n", ""),
            (" \n", "", " \n"),
        ] {
            let cmd = template(body);
            assert_eq!(expand(&cmd, args), expected);
            assert_eq!(expand_bounded(&cmd, args).unwrap(), expected);
            assert_eq!(
                expand_with_limit(&cmd, args, expected.len()).unwrap(),
                expected
            );
            if !expected.is_empty() {
                assert!(expand_with_limit(&cmd, args, expected.len() - 1).is_err());
            }
        }
    }

    #[test]
    fn bounded_expansion_default_limit_is_inclusive() {
        let mut cmd = template(&ARGUMENTS.repeat(1024));
        let args = "é".repeat(512);
        assert_eq!(
            expand_bounded(&cmd, &args).unwrap(),
            "é".repeat(MAX_EXPANDED_TASK_BYTES / 2)
        );

        cmd.template.insert(0, '!');
        let error = expand_bounded(&cmd, &args).unwrap_err().to_string();
        assert!(error.contains("1048577"), "{error}");
        assert!(error.contains("1048576 byte input budget"), "{error}");
        assert!(!error.contains(&args));
        // Existing callers still receive the full text from the compatible API.
        assert_eq!(expand(&cmd, &args).len(), MAX_EXPANDED_TASK_BYTES + 1);
    }

    #[test]
    fn small_expansion_budget_rejects_amplification_without_echoing_arguments() {
        let cmd = template(&format!("x{}", ARGUMENTS.repeat(64)));
        let args = "PRIVATE_TEMPLATE_ARGUMENTS";
        let error = expand_with_limit(&cmd, args, 32).unwrap_err().to_string();
        assert!(error.contains("32 byte input budget"), "{error}");
        assert!(!error.contains(args));
        // Count the final output: a large template can shrink below the budget.
        assert_eq!(expand_with_limit(&cmd, " \t", 1).unwrap(), "x");
        assert!(expand_with_limit(&cmd, "", 0).is_err());
    }

    #[test]
    fn expansion_byte_accounting_checks_integer_boundaries() {
        let marker_bytes = ARGUMENTS.len();
        assert_eq!(expanded_len(usize::MAX, 0, 0), Some(usize::MAX));
        assert_eq!(expanded_len(usize::MAX - 3, 0, 1), Some(usize::MAX));
        assert_eq!(expanded_len(marker_bytes, 1, usize::MAX), Some(usize::MAX));
        assert_eq!(expanded_len(2 * marker_bytes, 2, 0), Some(0));
        // Both append additions, replacement multiplication/addition, and marker accounting.
        assert_eq!(expanded_len(usize::MAX - 1, 0, 1), None);
        assert_eq!(expanded_len(usize::MAX - 2, 0, 1), None);
        assert_eq!(expanded_len(2 * marker_bytes, 2, usize::MAX / 2 + 1), None);
        assert_eq!(expanded_len(marker_bytes + 1, 1, usize::MAX), None);
        assert_eq!(
            expanded_len(usize::MAX, usize::MAX / marker_bytes + 1, 0),
            None
        );
        assert_eq!(expanded_len(marker_bytes - 1, 1, 0), None);
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
        let review = commands.iter().find(|c| c.name == "a:review").unwrap();
        assert_eq!(
            review.description.as_deref(),
            Some("Review the current diff")
        );
        assert_eq!(review.argument_hint.as_deref(), Some("[focus]"));

        // `--command a:review --args security` 展开成一条任务消息。
        let message = expand(review, "security");
        assert!(message.contains("focus on: security"), "{message}");
        assert!(!message.contains("$ARGUMENTS"));
        assert!(message.contains("Review `git diff`"));
        // 无参数时占位符展开为空。
        assert!(expand(review, "").contains("focus on: ."));
        // 无 $ARGUMENTS 占位符时参数追加到末尾（Claude Code 约定）。
        let deploy = commands.iter().find(|c| c.name == "b:deploy").unwrap();
        assert_eq!(expand(deploy, "--force"), "push to prod\n\n--force");
        assert_eq!(expand(deploy, ""), "push to prod");
    }

    #[test]
    fn skips_invalid_files_and_keeps_same_named_templates_from_each_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        command_file(&a, "broken.md", "---\ndescription: [unclosed\n---\nbody\n");
        command_file(&a, "dup.md", "first plugin\n");
        command_file(&a, "not-frontmatter.md", "plain body without frontmatter\n");
        let b = dir.path().join("b");
        command_file(&b, "dup.md", "second plugin\n");
        let set = PluginSet {
            plugins: vec![plugin(&a, "a"), plugin(&b, "b")],
            skipped: Vec::new(),
        };
        let (commands, skipped) = collect_commands(&set);
        let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
        assert_eq!(skipped.len(), 1, "只跳过坏 YAML：{reasons:?}");
        assert!(
            skipped
                .iter()
                .any(|s| s.path.file_name().is_some_and(|f| f == "broken.md")),
            "坏 YAML 要可见：{reasons:?}"
        );
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.name.ends_with(":dup"))
                .map(|c| (c.name.as_str(), c.template.as_str()))
                .collect::<Vec<_>>(),
            vec![("a:dup", "first plugin"), ("b:dup", "second plugin")],
            "插件命名空间保留两个模板"
        );
        assert!(!names.contains(&"dup"), "不得提供不明确的裸名称");
        let plain = commands
            .iter()
            .find(|c| c.name == "a:not-frontmatter")
            .unwrap();
        assert_eq!(plain.description, None, "无 frontmatter 容忍为正文");
        assert!(!names.contains(&"a:broken"), "坏 YAML 跳过：{names:?}");
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

    // macOS filesystems reject non-UTF-8 filenames at creation time.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_template_filename_has_no_lossy_alias() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a");
        command_file(&root, "review.md", "review task");
        let invalid = root
            .join(NAMESPACE)
            .join("commands")
            .join(OsString::from_vec(b"review-\xff.md".to_vec()));
        std::fs::write(&invalid, "unaddressable task").unwrap();
        let set = PluginSet {
            plugins: vec![plugin(&root, "a")],
            skipped: Vec::new(),
        };

        let (commands, skipped) = collect_commands(&set);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "a:review");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, invalid);
        assert!(skipped[0].reason.contains("filename must be UTF-8"));
    }

    /// R12 / S18：坏 symlink、超长正文都有带 path + 来源的可见诊断；
    /// 非 Markdown 与指向目录的链接属"不匹配"，静默。
    #[cfg(unix)]
    #[test]
    fn broken_and_oversized_command_files_are_reported() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        command_file(&a, "good.md", "fine body\n");
        command_file(
            &a,
            "huge.md",
            &"x".repeat(MAX_COMMAND_FILE_BYTES as usize + 1),
        );
        let commands_dir = a.join(NAMESPACE).join("commands");
        symlink(
            dir.path().join("nowhere.md"),
            commands_dir.join("dangling.md"),
        )
        .unwrap();
        std::fs::write(commands_dir.join("notes.txt"), "not a command").unwrap();
        std::fs::create_dir(commands_dir.join("dir.md")).unwrap();
        let set = PluginSet {
            plugins: vec![plugin(&a, "a")],
            skipped: Vec::new(),
        };

        let (commands, skipped) = collect_commands(&set);
        assert_eq!(
            commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a:good"]
        );
        let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
        assert!(
            skipped.iter().any(|s| {
                s.path == commands_dir.join("huge.md") && s.reason.contains("over the")
            }),
            "超长正文必须可见：{reasons:?}"
        );
        assert!(
            skipped.iter().any(|s| {
                s.path == commands_dir.join("dangling.md") && s.reason.contains("broken symlink")
            }),
            "坏 symlink 必须可见：{reasons:?}"
        );
        assert_eq!(skipped.len(), 2, "不匹配的条目不该产生诊断：{reasons:?}");
        // 诊断包含来源标签与解析后的绝对路径。
        for skipped in &skipped {
            assert!(skipped.reason.contains("plugin `a`"), "{skipped:?}");
            assert!(
                skipped
                    .reason
                    .contains(&crate::plugin::located(&skipped.path)),
                "{skipped:?} 应含绝对路径"
            );
        }
    }

    /// commands 目录读不了（权限）要报，不能整插件静默消失。
    #[cfg(unix)]
    #[test]
    fn unreadable_commands_dir_is_reported() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let plugin = plugin(&a, "a");
        let commands_dir = a.join(NAMESPACE).join("commands");
        command_file(&a, "hidden.md", "body\n");
        std::fs::set_permissions(&commands_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let set = PluginSet {
            plugins: vec![plugin],
            skipped: Vec::new(),
        };
        let (commands, skipped) = collect_commands(&set);
        std::fs::set_permissions(&commands_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !commands.is_empty() {
            return; // 以 root 运行时权限位不生效，跳过本用例。
        }
        assert!(
            skipped.iter().any(|s| {
                s.path == commands_dir && s.reason.contains("failed to read commands dir")
            }),
            "{skipped:?}"
        );
    }
}
