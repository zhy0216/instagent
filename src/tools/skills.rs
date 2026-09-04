//! `SkillsSource`：规范组件 skills 的运行时（第三版 §2.6）。只暴露一个工具
//! `load_skill(name, file?)`；启动时只收集 name + description 进系统提示
//! （由 `16`/`18` 接线），调用时才读正文或 `references/` 文件。
//!
//! 发现范围：每个启用插件的 `skills/`（只看一层子目录，含 `SKILL.md` 的子目录
//! 是一个 skill，不递归）+ `~/.agents/skills/` + `<project>/.agents/skills/`。
//! 插件里的 skill 命名 `<plugin>:<skill>`（goose `namespaced_component_name`）。
//! frontmatter 按 Agent Skills 规范校验（description 必须非空），无效 skill
//! 跳过不报错（warn 日志）。
//!
//! 资源与路径边界（计划 S10 / todo 04）：SKILL.md 与 supporting file 均受
//! [`MAX_SKILL_FILE_BYTES`] 单文件上限（metadata 预检 + `take(MAX+1)` 读后
//! 兜底增长竞态）；supporting file 明确 **no-follow**——目标本身是符号链接
//! 时拒绝读取（这是纵深防御，不是 containment 承诺，见 ADR 0003 D6：
//! 路径组件仍按字面解析，隔离由外部 sandbox 负责）。调用时读取经
//! [`builtin_fs::spawn_tool`] 移出 current-thread async 路径并检查取消。

use std::collections::HashSet;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::plugin::discovery::agents_dir;
use crate::plugin::manifest::namespaced_component_name;
use crate::plugin::PluginSet;
use crate::tools::builtin::fs as builtin_fs;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

/// `description` 上限（Agent Skills 规范）。
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// SKILL.md 与 supporting file 的单文件字节上限；超限在边界报可诊断错误，
/// 发现阶段则跳过该 skill（warn 日志）。
pub const MAX_SKILL_FILE_BYTES: u64 = 1024 * 1024;

/// SKILL.md frontmatter（Agent Skills 规范）。`allowed-tools` 等未知字段忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// 必需，1~64，小写字母数字和 `-`，必须等于目录名。
    pub name: String,
    /// 必需，非空，≤1024。
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// 一个已发现 skill 的元信息（进系统提示的那一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// `<plugin>:<skill>` 或裸名（用户/项目目录来源）。
    pub name: String,
    pub description: String,
    /// skill 目录（含 SKILL.md）。
    pub root: PathBuf,
}

#[derive(Debug, Default)]
pub struct SkillsSource {
    pub skills: Vec<SkillMeta>,
}

impl SkillsSource {
    /// 发现 + frontmatter 校验（无效跳过不报错）。同名先到先得，保持确定性。
    pub fn discover(plugins: &PluginSet, cwd: &Path) -> crate::Result<SkillsSource> {
        let mut source = SkillsSource::default();
        let mut seen: HashSet<String> = HashSet::new();

        for plugin in plugins.iter() {
            scan_skills_root(
                &plugin.root.join("skills"),
                Some(&plugin.manifest.name),
                &mut seen,
                &mut source,
            );
        }
        scan_skills_root(&agents_dir()?.join("skills"), None, &mut seen, &mut source);
        scan_skills_root(
            &cwd.join(".agents").join("skills"),
            None,
            &mut seen,
            &mut source,
        );
        Ok(source)
    }

    fn find(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.iter().find(|skill| skill.name == name)
    }
}

/// 一层子目录扫描：每个含 `SKILL.md` 的子目录是一个 skill，不递归。
/// `namespace` 是插件名（拼 `<plugin>:<skill>`），用户/项目根传 `None`。
fn scan_skills_root(
    root: &Path,
    namespace: Option<&str>,
    seen: &mut HashSet<String>,
    out: &mut SkillsSource,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return; // 根目录不存在不算错误
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let dir_name = match dir.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let path = dir.join("SKILL.md");
        // 有界读取：缺文件与超上限都跳过（没有 SKILL.md 的普通子目录不算无效）。
        let Ok(text) = read_skill_file(&path) else {
            continue;
        };
        let name = namespace.map_or_else(
            || dir_name.clone(),
            |plugin| namespaced_component_name(plugin, &dir_name),
        );
        match validate_skill(&text, &dir_name) {
            Ok(frontmatter) => {
                if !seen.insert(name.clone()) {
                    tracing::warn!("skill `{name}` 重复出现，跳过后到的 {}", dir.display());
                    continue;
                }
                out.skills.push(SkillMeta {
                    name,
                    description: frontmatter.description,
                    root: dir,
                });
            }
            Err(err) => tracing::warn!("跳过无效 skill `{}`：{err:#}", dir.display()),
        }
    }
}

/// frontmatter 解析 + 规范校验（name 字符集 / description 长度 / name == 目录名）。
fn validate_skill(text: &str, dir_name: &str) -> crate::Result<SkillFrontmatter> {
    let (frontmatter, _body) = parse_frontmatter::<SkillFrontmatter>(text, false)?;
    if !is_valid_skill_name(&frontmatter.name) {
        bail!(
            "name `{}` 不符合规范（1~64 位小写字母、数字或 `-`）",
            frontmatter.name
        );
    }
    if frontmatter.name != dir_name {
        bail!("name `{}` 与目录名 `{dir_name}` 不一致", frontmatter.name);
    }
    if frontmatter.description.trim().is_empty() {
        bail!("description 不能为空");
    }
    if frontmatter.description.chars().count() > MAX_DESCRIPTION_CHARS {
        bail!(
            "description 超过 {MAX_DESCRIPTION_CHARS} 字符（实际 {}）",
            frontmatter.description.chars().count()
        );
    }
    Ok(frontmatter)
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 相对路径安全校验：禁止绝对路径、`..`、盘符外组件（防目录逃逸）。
fn is_safe_relative_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
}

#[async_trait]
impl ToolSource for SkillsSource {
    fn id(&self) -> &str {
        "skills"
    }

    /// 只有一个 `load_skill`；工具描述文本抄自 goose
    /// `crates/goose/src/skills/client.rs:91~103`（commit `4ad43df`）。
    async fn list(&self) -> Vec<ToolSpec> {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to load. Use \"skill-name/path\" to load a supporting file."
                },
                "file": {
                    "type": "string",
                    "description": "Optional supporting file to load instead of SKILL.md, e.g. \"references/setup.md\"."
                }
            }
        });
        vec![ToolSpec {
            // 抄 goose client.rs:93~99 的 description 文本（搬运注明出处）。
            description: "Load a skill's full content into your context so you can follow its instructions.\n\n\
             Skills are listed in your system instructions. When you need to use one, \
             load it first to get the detailed instructions.\n\n\
             Examples:\n\
             - load_skill(name: \"gdrive\") → Loads the gdrive skill instructions\n\
             - load_skill(name: \"my-skill\", args: \"the arguments for the skill\") → Loads a skill with arguments\n\
             - load_skill(name: \"my-skill/template.md\") → Loads a supporting file"
                .to_string(),
            input_schema: schema,
            name: "load_skill".to_string(),
            read_only: true,
        }]
    }

    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput {
        if name != "load_skill" {
            return ToolOutput::err(format!("unknown tool: {name}"));
        }
        let Some(query) = input.get("name").and_then(Value::as_str) else {
            return ToolOutput::err("Missing required parameter: name".to_string());
        };
        // 兼容 goose 的 "skill-name/path" 形态：斜杠后缀视为 supporting file。
        let (skill_name, inline_file) = match query.split_once('/') {
            Some((skill, file)) => (skill, Some(file)),
            None => (query, None),
        };
        let file = inline_file.or_else(|| input.get("file").and_then(Value::as_str));

        let Some(skill) = self.find(skill_name) else {
            return ToolOutput::err(format!("Unknown skill: {skill_name}"));
        };
        let root = skill.root.clone();
        let cancel = ctx.cancel.clone();
        match file {
            Some(file) => {
                let file = file.to_string();
                builtin_fs::spawn_tool(move || load_supporting_file_sync(&root, &file, &cancel))
                    .await
            }
            None => builtin_fs::spawn_tool(move || load_skill_body_sync(&root, &cancel)).await,
        }
    }
}

/// 有界读取单个 skill 文件（[`MAX_SKILL_FILE_BYTES`]）：metadata 预检 +
/// `take(MAX+1)` 读后兜底 metadata/read 增长竞态，超限返回可诊断错误文案。
fn read_skill_file(path: &Path) -> Result<String, String> {
    let fail = |e: std::io::Error| format!("Failed to read {}: {e}", path.display());
    let meta = std::fs::metadata(path).map_err(fail)?;
    if meta.len() > MAX_SKILL_FILE_BYTES {
        return Err(format!(
            "Failed to read {}: file too large ({} bytes, limit {MAX_SKILL_FILE_BYTES} bytes)",
            path.display(),
            meta.len()
        ));
    }
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .map_err(fail)?
        .take(MAX_SKILL_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(fail)?;
    if buf.len() as u64 > MAX_SKILL_FILE_BYTES {
        return Err(format!(
            "Failed to read {}: file grew past {MAX_SKILL_FILE_BYTES} bytes during read",
            path.display()
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| format!("Failed to read {}: not valid UTF-8", path.display()))
}

fn load_skill_body_sync(root: &Path, cancel: &CancellationToken) -> ToolOutput {
    let path = root.join("SKILL.md");
    if cancel.is_cancelled() {
        return builtin_fs::cancelled_output(&path);
    }
    let text = match read_skill_file(&path) {
        Ok(text) => text,
        Err(message) => return ToolOutput::err(message),
    };
    match parse_frontmatter::<SkillFrontmatter>(&text, false) {
        Ok((_frontmatter, body)) => ToolOutput::ok(body),
        Err(err) => ToolOutput::err(format!("Failed to parse {}: {err:#}", path.display())),
    }
}

fn load_supporting_file_sync(root: &Path, file: &str, cancel: &CancellationToken) -> ToolOutput {
    if !is_safe_relative_path(file) {
        return ToolOutput::err(format!(
            "Invalid supporting file path: {file} (must be a relative path inside the skill)"
        ));
    }
    let target = root.join(file);
    if cancel.is_cancelled() {
        return builtin_fs::cancelled_output(&target);
    }
    // no-follow：目标本身是符号链接时拒绝（防 supporting file 经链接逃出
    // skill root 的纵深防御；完整 containment 不是应用层承诺，见 ADR 0003 D6）。
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return ToolOutput::err(format!(
                "Refusing to follow symlink: {} (supporting files must be regular files inside the skill)",
                target.display()
            ));
        }
        Err(err) => return ToolOutput::err(format!("Failed to read {}: {err}", target.display())),
        Ok(_) => {}
    }
    match read_skill_file(&target) {
        Ok(content) => ToolOutput::ok(content),
        Err(message) => ToolOutput::err(message),
    }
}

/// 拆分并解析 `---` frontmatter，返回（元信息，正文）。
/// 缺失 frontmatter：`missing_ok` 为真时容忍（元信息取 Default，整篇是正文），
/// 否则报错。
pub fn parse_frontmatter<F>(text: &str, missing_ok: bool) -> crate::Result<(F, String)>
where
    F: serde::de::DeserializeOwned + Default,
{
    let text = text.trim_start_matches('\u{feff}');
    let rest = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None if missing_ok => return Ok((F::default(), text.trim().to_string())),
        None => bail!("missing a `---` frontmatter block"),
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
        bail!("unterminated frontmatter: missing closing `---`");
    };
    let frontmatter: F = serde_yaml::from_str(&fm_lines.join("\n"))
        .map_err(|err| anyhow::anyhow!("invalid frontmatter YAML: {err}"))?;
    Ok((frontmatter, rest[body_start..].trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use crate::settings::Settings;
    use std::sync::MutexGuard;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    /// 串行化进程级环境变量并隔离 `~/.agents`（约定同 `discovery.rs` 测试）。
    struct Env {
        _guard: MutexGuard<'static, ()>,
        agents: TempDir,
    }

    fn isolated_agents() -> Env {
        let guard = crate::config::lock_env();
        let agents = TempDir::new().unwrap();
        std::env::set_var("INSTAGENT_AGENTS_DIR", agents.path());
        Env {
            _guard: guard,
            agents,
        }
    }

    fn ctx_in(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    fn write_plugin(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
    }

    /// 规范最小示例 skill：目录 + SKILL.md（frontmatter 的 name 默认等于目录名）。
    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    fn skills_env() -> (Env, TempDir, PathBuf) {
        let env = isolated_agents();
        // 启用插件 alpha，带一个 plugin skill。
        let plugin_root = env.agents.path().join("plugins").join("alpha");
        write_plugin(&plugin_root, "alpha");
        write_skill(
            &plugin_root.join("skills").join("review"),
            "review",
            "Review the current diff",
            "Run `git diff` and report findings.",
        );
        // 用户级 skill。
        write_skill(
            &env.agents.path().join("skills").join("gdrive"),
            "gdrive",
            "Google Drive access",
            "Use drive commands.",
        );
        let cwd = TempDir::new().unwrap();
        // 项目级 skill + references 支持文件。
        let project_skill = cwd.path().join(".agents").join("skills").join("pdf");
        write_skill(
            &project_skill,
            "pdf",
            "Work with PDF files",
            "Body of the pdf skill.",
        );
        std::fs::create_dir_all(project_skill.join("references")).unwrap();
        std::fs::write(
            project_skill.join("references").join("setup.md"),
            "reference setup content\n",
        )
        .unwrap();
        (env, cwd, plugin_root)
    }

    async fn load(source: &SkillsSource, cwd: &Path, input: Value) -> ToolOutput {
        source.call("load_skill", input, &ctx_in(cwd)).await
    }

    #[test]
    fn discovers_plugin_user_and_project_skills() {
        let (env, cwd, _plugin) = skills_env();
        let plugins =
            crate::plugin::discovery::discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        let source = SkillsSource::discover(&plugins, cwd.path()).unwrap();
        let names: Vec<&str> = source.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha:review", "gdrive", "pdf"]);
        assert_eq!(
            source.find("alpha:review").unwrap().root,
            env.agents
                .path()
                .join("plugins")
                .join("alpha")
                .join("skills")
                .join("review")
        );
        assert_eq!(
            source.find("pdf").unwrap().description,
            "Work with PDF files"
        );
    }

    #[test]
    fn invalid_skills_are_skipped_without_error() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let skills = env.agents.path().join("skills");

        // 1) name 与目录名不符。
        write_skill(&skills.join("wrong-name"), "other", "desc", "body");
        // 2) description 超长（1025 字符）。
        write_skill(
            &skills.join("too-long"),
            "too-long",
            &"d".repeat(1025),
            "body",
        );
        // 3) 缺 name。
        std::fs::create_dir_all(skills.join("no-name")).unwrap();
        std::fs::write(
            skills.join("no-name").join("SKILL.md"),
            "---\ndescription: x\n---\n",
        )
        .unwrap();
        // 4) 非法字符（大写）。
        write_skill(&skills.join("Bad"), "Bad", "desc", "body");
        // 5) 不递归：嵌套目录里的 SKILL.md 不算独立 skill，外层无 SKILL.md 也不报错。
        write_skill(
            &skills.join("outer").join("inner"),
            "inner",
            "nested desc",
            "nested body",
        );
        // 6) 没有 SKILL.md 的普通子目录直接忽略。
        std::fs::create_dir_all(skills.join("not-a-skill")).unwrap();
        // 7) description 为空 / 全空白（规范要求的 non-empty）。
        std::fs::create_dir_all(skills.join("empty-desc")).unwrap();
        std::fs::write(
            skills.join("empty-desc").join("SKILL.md"),
            "---\nname: empty-desc\ndescription: \"\"\n---\nbody\n",
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("blank-desc")).unwrap();
        std::fs::write(
            skills.join("blank-desc").join("SKILL.md"),
            "---\nname: blank-desc\ndescription: \"   \"\n---\nbody\n",
        )
        .unwrap();
        // 8) SKILL.md 超过单文件上限：发现阶段有界读取后跳过。
        std::fs::create_dir_all(skills.join("too-big")).unwrap();
        std::fs::write(
            skills.join("too-big").join("SKILL.md"),
            vec![b'x'; MAX_SKILL_FILE_BYTES as usize + 1],
        )
        .unwrap();
        // 9) 合法对照 + 恰 1024 字符 description 的边界样本。
        write_skill(&skills.join("good"), "good", "fine", "body");
        write_skill(&skills.join("edge"), "edge", &"d".repeat(1024), "body");

        let plugins = PluginSet::default();
        let source = SkillsSource::discover(&plugins, cwd.path()).unwrap();
        let names: Vec<&str> = source.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["edge", "good"], "无效 skill 全部跳过，不报错");
    }

    #[tokio::test]
    async fn load_skill_returns_body_and_reference_file() {
        let (_env, cwd, _plugin) = skills_env();
        let plugins =
            crate::plugin::discovery::discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        let source = SkillsSource::discover(&plugins, cwd.path()).unwrap();

        let out = load(&source, cwd.path(), json!({"name": "alpha:review"})).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.text, "Run `git diff` and report findings.");

        let out = load(
            &source,
            cwd.path(),
            json!({"name": "pdf", "file": "references/setup.md"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.text, "reference setup content\n");

        // goose 风格 "skill-name/path" 也接受。
        let out = load(
            &source,
            cwd.path(),
            json!({"name": "pdf/references/setup.md"}),
        )
        .await;
        assert_eq!(out.text, "reference setup content\n");
    }

    #[tokio::test]
    async fn load_skill_rejects_unknown_and_traversal() {
        let (_env, cwd, _plugin) = skills_env();
        let plugins =
            crate::plugin::discovery::discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        let source = SkillsSource::discover(&plugins, cwd.path()).unwrap();

        let out = load(&source, cwd.path(), json!({"name": "nope"})).await;
        assert!(out.is_error);
        assert!(out.text.contains("Unknown skill"));

        let out = load(&source, cwd.path(), json!({})).await;
        assert!(out.text.contains("Missing required parameter"));

        let out = load(
            &source,
            cwd.path(),
            json!({"name": "pdf", "file": "../../etc/passwd"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("Invalid supporting file path"));

        let out = load(
            &source,
            cwd.path(),
            json!({"name": "pdf", "file": "/etc/passwd"}),
        )
        .await;
        assert!(out.is_error);

        let out = load(
            &source,
            cwd.path(),
            json!({"name": "pdf", "file": "references/missing.md"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("Failed to read"));
    }

    #[tokio::test]
    async fn list_exposes_only_load_skill() {
        let source = SkillsSource::default();
        let specs = source.list().await;
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "load_skill");
        assert!(specs[0].read_only);
        assert!(specs[0]
            .description
            .contains("Load a skill's full content into your context"));
        // 抄文保留 goose 的示例行（出处见 client.rs:91~103 注释）。
        assert!(specs[0]
            .description
            .contains("load_skill(name: \"gdrive\")"));
        assert_eq!(source.id(), "skills");
    }

    #[test]
    fn parse_frontmatter_reads_optional_fields_and_body() {
        let text = "---\nname: my-skill\ndescription: does things\nlicense: MIT\n\
                    allowed-tools: [read_file]\nmetadata:\n  x: 1\n---\n\nDo it.\n";
        let (fm, body) = parse_frontmatter::<SkillFrontmatter>(text, false).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "does things");
        assert_eq!(fm.license.as_deref(), Some("MIT"));
        assert_eq!(fm.metadata, Some(json!({"x": 1})));
        assert_eq!(body, "Do it.");
    }

    #[test]
    fn parse_frontmatter_rejects_malformed_blocks() {
        assert!(parse_frontmatter::<SkillFrontmatter>("no frontmatter", false).is_err());
        assert!(
            parse_frontmatter::<SkillFrontmatter>("---\nname: x\ndescription: y\n", false).is_err()
        );
        // 缺 description。
        assert!(parse_frontmatter::<SkillFrontmatter>("---\nname: x\n---\nbody\n", false).is_err());
        // 非法 YAML。
        assert!(
            parse_frontmatter::<SkillFrontmatter>("---\nname: [unclosed\n---\n", false).is_err()
        );
    }

    #[test]
    fn name_and_path_validation_helpers() {
        assert!(is_valid_skill_name("my-skill-2"));
        assert!(!is_valid_skill_name("My-skill"));
        assert!(!is_valid_skill_name("my_skill"));
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name(&"a".repeat(65)));
        assert!(is_safe_relative_path("references/setup.md"));
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("../escape"));
        assert!(!is_safe_relative_path("a/../../b"));
    }

    #[test]
    fn validate_skill_requires_nonempty_description() {
        let empty = "---\nname: good\ndescription: \"\"\n---\nbody\n";
        let err = validate_skill(empty, "good").unwrap_err();
        assert!(format!("{err:#}").contains("description"), "{err:#}");

        let blank = "---\nname: good\ndescription: \"   \"\n---\nbody\n";
        assert!(validate_skill(blank, "good").is_err());

        let ok = "---\nname: good\ndescription: does things\n---\nbody\n";
        assert!(validate_skill(ok, "good").is_ok());
    }

    /// SKILL.md 超过单文件上限：加载报可诊断错误，不整读入内存。
    #[test]
    fn load_skill_body_rejects_oversized_skill_md() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("big");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            vec![b'x'; MAX_SKILL_FILE_BYTES as usize + 1],
        )
        .unwrap();

        let cancel = CancellationToken::new();
        let out = load_skill_body_sync(&skill, &cancel);
        assert!(out.is_error);
        assert!(out.text.contains("too large"), "{}", out.text);
    }

    /// supporting file 超过单文件上限：报可诊断错误。
    #[tokio::test]
    async fn load_skill_supporting_file_respects_size_limit() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let skill = cwd.path().join(".agents").join("skills").join("big");
        write_skill(&skill, "big", "Has a big reference", "body");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(
            skill.join("references").join("big.md"),
            vec![b'x'; MAX_SKILL_FILE_BYTES as usize + 1],
        )
        .unwrap();

        let source = SkillsSource::discover(&PluginSet::default(), cwd.path()).unwrap();
        let out = load(
            &source,
            cwd.path(),
            json!({"name": "big", "file": "references/big.md"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("too large"), "{}", out.text);
        let _ = env;
    }

    /// supporting file no-follow：目标本身是符号链接时拒绝，不跟随逃出。
    #[cfg(unix)]
    #[tokio::test]
    async fn load_skill_supporting_file_rejects_symlink() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let skill = cwd.path().join(".agents").join("skills").join("linky");
        write_skill(&skill, "linky", "Has a symlinked reference", "body");
        let secret = cwd.path().join("secret.txt");
        std::fs::write(&secret, "top secret\n").unwrap();
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::os::unix::fs::symlink(&secret, skill.join("references").join("link.md")).unwrap();

        let source = SkillsSource::discover(&PluginSet::default(), cwd.path()).unwrap();
        let out = load(
            &source,
            cwd.path(),
            json!({"name": "linky", "file": "references/link.md"}),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text.contains("Refusing to follow symlink"),
            "{}",
            out.text
        );
        let _ = env;
    }

    /// 预取消：load_skill 立即返回带 `cancelled` 的错误。
    #[tokio::test]
    async fn load_skill_precancelled_returns_cancelled_error() {
        let (_env, cwd, _plugin) = skills_env();
        let plugins =
            crate::plugin::discovery::discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        let source = SkillsSource::discover(&plugins, cwd.path()).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let ctx = ToolCtx {
            cwd: cwd.path().to_path_buf(),
            cancel: token,
        };

        let out = source
            .call("load_skill", json!({"name": "pdf"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
    }
}
