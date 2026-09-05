//! 插件发现与启用（第三版 §2.10）。
//!
//! 扫描顺序：`~/.agents/plugins/`（用户）→ `<project>/.agents/plugins/`（项目）→
//! 配置 `plugins` 额外路径（`01`）→ 运行时 `--plugin PATH`。同名插件按层覆盖：
//! CLI > 配置额外路径 > 项目 > 用户（[`PluginSource`] 注释）；层内按扫描顺序
//! 先到先得。额外路径与 CLI 参数自动识别：目录本身含 `plugin.json` 视为单个
//! 插件，否则视为插件根目录逐个子目录扫描。manifest 校验失败的目录记录原因并
//! 跳过，不中断整体发现。
//!
//! 目录枚举统一走 `scan_path`，把"不匹配"（散文件、没有 `plugin.json` 的
//! 目录）与真失败（逐条 IO 错误、坏 symlink、无权限）分开：前者静默跳过，
//! 后者汇总成带来源与解析后绝对路径的 skipped 诊断（R12），装配层经
//! [`PluginSet::skipped`] 提示给用户，绝不无声消失。
//!
//! 启用判定用 `01` 三层合并后的 settings，三态按 ADR 0003 D5 由
//! [`Settings::whitelist`] 表达：缺失 = 不表态（`disabledPlugins` 黑名单）、
//! 非空 = 白名单、显式 `[]` = 禁用全部。判定逻辑在 [`plugin_enabled`]，
//! discovery 与 bundled 共用。

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use crate::plugin::located;
use crate::plugin::manifest::read_manifest;
use crate::plugin::plugin_enabled;
use crate::plugin::Plugin;
use crate::plugin::PluginSource;
use crate::settings::Settings;

/// 启用的插件集合；迭代得到 {manifest, root, source}，供 `06/07/10/13/15` 使用。
#[derive(Debug, Clone, Default)]
pub struct PluginSet {
    /// 按插件名升序。
    pub plugins: Vec<Plugin>,
    /// 目录枚举或 manifest 校验（`04`）失败、被发现但跳过的目录及原因
    /// （第三版 §2.10 F1、R12）。`reason` 内含来源显示名与解析后的绝对路径。
    pub skipped: Vec<SkippedPlugin>,
}

/// 一个被跳过的插件目录：路径 + manifest 校验失败原因（第三版 §2.10 F1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPlugin {
    pub path: PathBuf,
    pub reason: String,
}

impl PluginSet {
    pub fn iter(&self) -> std::slice::Iter<'_, Plugin> {
        self.plugins.iter()
    }

    /// 按裸名查找；重名（`plugin/name` 消歧）由 `10` 在 provider 层处理。
    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.manifest.name == name)
    }
}

/// 同名覆盖的层序（比 [`PluginSource`] 更细：配置额外路径与 CLI 参数覆盖力
/// 同级但 CLI 后扫描、优先覆盖，来源 kind 仍各自独立，见 E8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    User = 0,
    Project = 1,
    Extra = 2,
    Cli = 3,
}

impl Layer {
    fn source(self) -> PluginSource {
        match self {
            Layer::User => PluginSource::User,
            Layer::Project => PluginSource::Project,
            Layer::Extra => PluginSource::Extra,
            Layer::Cli => PluginSource::Cli,
        }
    }

    /// 用户显式声明的路径（配置 `plugins` / `--plugin`）：缺失要诊断，
    /// 用户/项目根缺失才属正常（首次运行）。
    fn is_explicit(self) -> bool {
        matches!(self, Layer::Extra | Layer::Cli)
    }
}

struct Candidate {
    plugin: Plugin,
    layer: Layer,
}

/// `~/.agents` 根目录；`INSTAGENT_AGENTS_DIR` 覆盖（测试用，与 `config.rs`
/// 的 `INSTAGENT_CONFIG_DIR` 同一约定）。`15` 的 skills 发现共用。
pub fn agents_dir() -> crate::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("INSTAGENT_AGENTS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    use etcetera::base_strategy::BaseStrategy as _;
    let strategy = etcetera::base_strategy::choose_base_strategy()?;
    Ok(strategy.home_dir().join(".agents"))
}

/// 扫描 `~/.agents/plugins/`、`<cwd>/.agents/plugins/`、配置 `plugins`
/// 额外路径与 `--plugin PATH`；应用三层 settings 的启用判定。
pub fn discover(
    cwd: &Path,
    settings: &Settings,
    extra_paths: &[PathBuf],
    cli_plugins: &[PathBuf],
) -> crate::Result<PluginSet> {
    let mut found: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut skipped = Vec::new();

    scan_path(
        &agents_dir()?.join("plugins"),
        Layer::User,
        &mut found,
        &mut skipped,
    );
    scan_path(
        &cwd.join(".agents").join("plugins"),
        Layer::Project,
        &mut found,
        &mut skipped,
    );
    for path in extra_paths {
        scan_path(path, Layer::Extra, &mut found, &mut skipped);
    }
    for path in cli_plugins {
        scan_path(path, Layer::Cli, &mut found, &mut skipped);
    }

    let plugins = found
        .into_values()
        .map(|candidate| candidate.plugin)
        .filter(|plugin| plugin_enabled(&plugin.manifest.name, settings))
        .collect();
    Ok(PluginSet { plugins, skipped })
}

/// `path` 含 `plugin.json` 则按单个插件目录注册，否则作为插件根目录按名称
/// 升序扫描其子目录（目录内先到先得，同名靠 [`Layer`] 覆盖）。
///
/// 枚举失败一律汇总为 skipped 诊断（R12）：根目录读不了（权限、IO、不是
/// 目录）、逐条 entry 读失败、坏 symlink、`plugin.json` 无法 stat 都要说话；
/// 只有"确实不匹配"（散文件、没有 `plugin.json` 的普通目录、指向文件的链接）
/// 静默。显式声明的路径（配置 / CLI）整条扫不到插件也记一条，覆盖目录被删、
/// 写错、指错位置（第三版 §5 P7）。
fn scan_path(
    path: &Path,
    layer: Layer,
    found: &mut BTreeMap<String, Candidate>,
    skipped: &mut Vec<SkippedPlugin>,
) {
    let source = layer.source();
    let mut problems: Vec<(PathBuf, String)> = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();

    match plugin_json_state(path) {
        PluginJson::Present => candidates.push(path.to_path_buf()),
        PluginJson::Unreadable(reason) => problems.push((path.to_path_buf(), reason)),
        PluginJson::Absent => match list_child_dirs(path) {
            Err(DirError::Missing) if !layer.is_explicit() => return,
            Err(DirError::Missing) => {
                let reason = if is_symlink(path) {
                    "plugin path is a broken symlink (target missing or unreadable)"
                } else {
                    "plugin path not found (deleted, moved, or misspelled)"
                };
                problems.push((path.to_path_buf(), reason.to_string()));
            }
            Err(DirError::Failed(reason)) => problems.push((path.to_path_buf(), reason)),
            Ok(ChildDirs { dirs, failures }) => {
                problems.extend(failures);
                candidates = dirs;
            }
        },
    }
    // 子目录里只留真正的插件目录；没有 `plugin.json` 属不匹配，读不到才诊断。
    candidates.retain(|dir| match plugin_json_state(dir) {
        PluginJson::Present => true,
        PluginJson::Absent => false,
        PluginJson::Unreadable(reason) => {
            problems.push((dir.clone(), reason));
            false
        }
    });
    if candidates.is_empty() && problems.is_empty() && layer.is_explicit() {
        problems.push((
            path.to_path_buf(),
            "no plugin found here or in its child directories \
             (plugin.json missing; empty or wrong directory?)"
                .to_string(),
        ));
    }
    for (at, reason) in problems {
        skipped.push(SkippedPlugin {
            path: at.clone(),
            reason: format!("{} [{}]: {reason}", source.display_name(), located(&at)),
        });
    }
    for dir in candidates {
        register(&dir, source, layer, found, skipped);
    }
}

/// 目录里 `plugin.json` 的状态：区分"没有"（不匹配）与"读不了"（权限 / IO）。
enum PluginJson {
    Present,
    Absent,
    Unreadable(String),
}

fn plugin_json_state(dir: &Path) -> PluginJson {
    let manifest = dir.join("plugin.json");
    match std::fs::metadata(&manifest) {
        Ok(meta) if meta.is_file() => PluginJson::Present,
        Ok(_) => PluginJson::Unreadable(format!("{} is not a regular file", manifest.display())),
        // `NotADirectory`：`dir` 根本不是目录，交给根目录枚举去报真实原因。
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            PluginJson::Absent
        }
        Err(err) => PluginJson::Unreadable(format!("cannot stat {}: {err}", manifest.display())),
    }
}

/// 根目录枚举的失败形状。
enum DirError {
    /// 目录不存在（含坏 symlink 的目标缺失）：层级决定是否算问题。
    Missing,
    /// 权限、不是目录、其它 IO 失败：任何层级都要诊断。
    Failed(String),
}

struct ChildDirs {
    dirs: Vec<PathBuf>,
    failures: Vec<(PathBuf, String)>,
}

/// 统一枚举插件根目录的子目录（R12）：坏 symlink 与逐条读取失败进
/// `failures`；散文件、指向文件的链接属不匹配，静默；安装内部状态目录
/// （`.replaced-*` 备份 / `.tmp-install` staging，见
/// [`crate::plugin::install::is_install_internal_dir`]）同样静默排除，不把
/// 正在安装或待恢复的副本发现成插件（I02）。返回按路径升序。
fn list_child_dirs(path: &Path) -> Result<ChildDirs, DirError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(DirError::Missing),
        Err(err) => {
            return Err(DirError::Failed(format!(
                "failed to read plugin root: {err}"
            )))
        }
    };
    let mut dirs = Vec::new();
    let mut failures = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            failures.push((
                path.to_path_buf(),
                "failed to read a dir entry of plugin root \
                 (permission denied, changed during scan, or unreadable directory?)"
                    .to_string(),
            ));
            continue;
        };
        let child = entry.path();
        if crate::plugin::install::is_install_internal_dir(&entry.file_name()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            failures.push((
                child.clone(),
                "failed to stat dir entry (unreadable directory?)".to_string(),
            ));
            continue;
        };
        let is_dir = if file_type.is_symlink() {
            match std::fs::metadata(&child) {
                Ok(meta) => meta.is_dir(),
                Err(err) => {
                    failures.push((child, format!("broken symlink: {err}")));
                    continue;
                }
            }
        } else {
            file_type.is_dir()
        };
        if is_dir {
            dirs.push(child);
        }
    }
    dirs.sort();
    Ok(ChildDirs { dirs, failures })
}

fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

fn register(
    dir: &Path,
    source: PluginSource,
    layer: Layer,
    found: &mut BTreeMap<String, Candidate>,
    skipped: &mut Vec<SkippedPlugin>,
) {
    let manifest = match read_manifest(dir) {
        Ok(manifest) => manifest,
        Err(err) => {
            skipped.push(SkippedPlugin {
                path: dir.to_path_buf(),
                reason: format!("{} [{}]: {err:#}", source.display_name(), located(dir)),
            });
            return;
        }
    };
    let name = manifest.name.clone();
    if found
        .get(&name)
        .is_some_and(|existing| existing.layer >= layer)
    {
        return;
    }
    found.insert(
        name,
        Candidate {
            plugin: Plugin {
                manifest,
                root: dir.to_path_buf(),
                source,
            },
            layer,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// 串行化进程级环境变量并隔离 `~/.agents`（测试不碰真实 HOME，
    /// 约定同 `config.rs` / `settings.rs`）。
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

    fn write_plugin(dir: &Path, name: &str, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    /// `<root>/<name>/` 形态的插件根目录。
    fn in_root(root: &Path, name: &str, version: &str) -> PathBuf {
        let dir = root.join(name);
        write_plugin(&dir, name, version);
        dir
    }

    fn user_plugins(env: &Env) -> PathBuf {
        env.agents.path().join("plugins")
    }

    fn project_plugins(cwd: &Path) -> PathBuf {
        cwd.join(".agents").join("plugins")
    }

    fn names(set: &PluginSet) -> Vec<&str> {
        set.iter().map(|p| p.manifest.name.as_str()).collect()
    }

    #[test]
    fn discovers_from_all_four_layers_with_sources() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        in_root(&user_plugins(&env), "alpha", "1.0.0");
        in_root(&project_plugins(cwd.path()), "beta", "1.0.0");
        let extras = TempDir::new().unwrap();
        in_root(extras.path(), "gamma", "1.0.0");
        let cli_dir = TempDir::new().unwrap();
        write_plugin(cli_dir.path(), "delta", "1.0.0"); // --plugin 直指插件目录

        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[extras.path().to_path_buf()],
            &[cli_dir.path().to_path_buf()],
        )
        .unwrap();
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
        assert_eq!(names(&set), ["alpha", "beta", "delta", "gamma"]);
        assert_eq!(set.get("alpha").unwrap().source, PluginSource::User);
        assert_eq!(set.get("beta").unwrap().source, PluginSource::Project);
        assert_eq!(set.get("gamma").unwrap().source, PluginSource::Extra);
        assert_eq!(set.get("delta").unwrap().source, PluginSource::Cli);
        assert_eq!(
            set.get("beta").unwrap().root,
            project_plugins(cwd.path()).join("beta")
        );
    }

    #[test]
    fn same_name_project_overrides_user_then_extra_then_cli() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        in_root(&user_plugins(&env), "shared", "1-user");
        in_root(&project_plugins(cwd.path()), "shared", "2-project");

        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert_eq!(set.plugins.len(), 1);
        let plugin = set.get("shared").unwrap();
        assert_eq!(plugin.manifest.version, "2-project");
        assert_eq!(plugin.source, PluginSource::Project);

        // 配置额外路径覆盖项目层。
        let extra = TempDir::new().unwrap();
        in_root(extra.path(), "shared", "3-extra");
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[extra.path().to_path_buf()],
            &[],
        )
        .unwrap();
        assert_eq!(set.get("shared").unwrap().manifest.version, "3-extra");

        // `--plugin` 覆盖额外路径；同一层内先列出的路径优先。
        let cli = TempDir::new().unwrap();
        write_plugin(cli.path(), "shared", "4-cli");
        let extra2 = TempDir::new().unwrap();
        in_root(extra2.path(), "shared", "3b-extra-later");
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[extra.path().to_path_buf(), extra2.path().to_path_buf()],
            &[cli.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(set.plugins.len(), 1);
        assert_eq!(set.get("shared").unwrap().manifest.version, "4-cli");
    }

    #[test]
    fn invalid_manifests_are_recorded_and_skipped() {
        let env = isolated_agents();
        let user = user_plugins(&env);
        in_root(&user, "good", "1.0.0");
        let no_schema = user.join("no-schema");
        std::fs::create_dir_all(&no_schema).unwrap();
        std::fs::write(
            no_schema.join("plugin.json"),
            r#"{"name":"no-schema","version":"1.0.0"}"#,
        )
        .unwrap();
        let broken = user.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("plugin.json"), "{ not json").unwrap();
        let empty = user.join("not-a-plugin");
        std::fs::create_dir_all(empty.clone()).unwrap();
        // 根目录里的散文件不是插件候选，也不报错。
        std::fs::write(user.join("README.md"), "stray file").unwrap();

        let cwd = TempDir::new().unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert_eq!(names(&set), ["good"]);
        let skipped: Vec<&Path> = set.skipped.iter().map(|s| s.path.as_path()).collect();
        assert!(skipped.contains(&&*no_schema), "{skipped:?}");
        assert!(skipped.contains(&&*broken), "{skipped:?}");
        assert!(
            !skipped.contains(&&*empty),
            "没有 plugin.json 的普通目录属\"不匹配\"，不该报：{skipped:?}"
        );
        assert!(set
            .skipped
            .iter()
            .any(|s| { s.path == no_schema && s.reason.contains("must declare `$schema`") }));
        assert!(set
            .skipped
            .iter()
            .any(|s| { s.path == broken && s.reason.contains("Failed to parse") }));
        // 诊断同时给出来源与解析后的绝对路径（E8）。
        for skipped in &set.skipped {
            assert!(
                skipped.reason.contains("user plugin dir"),
                "{skipped:?} 应标明来源层"
            );
            assert!(
                skipped.reason.contains(&located(&skipped.path)),
                "{skipped:?} 应含解析后的绝对路径"
            );
        }
    }

    #[test]
    fn whitelist_mode_enables_only_listed() {
        let env = isolated_agents();
        let user = user_plugins(&env);
        in_root(&user, "a", "1.0.0");
        in_root(&user, "b", "1.0.0");
        let settings = Settings {
            enabled_plugins: vec!["a".into()],
            disabled_plugins: vec!["a".into()], // 与白名单冲突时白名单说了算
            ..Settings::default()
        };
        let set = discover(env.agents.path(), &settings, &[], &[]).unwrap();
        assert_eq!(names(&set), ["a"]);
    }

    #[test]
    fn blacklist_mode_disables_only_listed() {
        let env = isolated_agents();
        let user = user_plugins(&env);
        in_root(&user, "a", "1.0.0");
        in_root(&user, "b", "1.0.0");
        let settings = Settings {
            disabled_plugins: vec!["b".into()],
            ..Settings::default()
        };
        let set = discover(env.agents.path(), &settings, &[], &[]).unwrap();
        assert_eq!(names(&set), ["a"]);
    }

    /// ADR 0003 D5 消费端接线：显式 `[]` = 禁用全部，缺失 = 不表态延续低层。
    #[test]
    fn explicit_empty_whitelist_disables_everything() {
        let config = TempDir::new().unwrap();
        let env = isolated_agents();
        std::env::set_var("INSTAGENT_CONFIG_DIR", config.path());
        let user_settings = config.path().join("settings.json");
        std::fs::write(&user_settings, r#"{"enabledPlugins":[]}"#).unwrap();
        in_root(&user_plugins(&env), "a", "1.0.0");
        in_root(&user_plugins(&env), "b", "1.0.0");

        let cwd = TempDir::new().unwrap();
        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&[][..]));
        let set = discover(cwd.path(), &settings, &[], &[]).unwrap();
        assert!(
            set.plugins.is_empty(),
            "显式 [] 必须禁用全部：{:?}",
            names(&set)
        );

        // 同一个键改成缺失：不表态 → 黑名单模式，全启用。
        std::fs::write(&user_settings, r#"{"disabledPlugins":[]}"#).unwrap();
        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings.whitelist(), None);
        let set = discover(cwd.path(), &settings, &[], &[]).unwrap();
        assert_eq!(names(&set), ["a", "b"]);
    }

    /// I01 消费端回归：用户层白名单仅 `a`、项目层禁用 `a` 时，合并终值
    /// 保持白名单模式（空集合），`b` 不因模式翻转成黑名单而被发现。
    #[test]
    fn lower_whitelist_cleared_by_higher_disabled_still_disables_others() {
        let config = TempDir::new().unwrap();
        let env = isolated_agents();
        std::env::set_var("INSTAGENT_CONFIG_DIR", config.path());
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        )
        .unwrap();
        let cwd = TempDir::new().unwrap();
        std::fs::create_dir_all(cwd.path().join(".config").join("instagent")).unwrap();
        std::fs::write(
            cwd.path()
                .join(".config")
                .join("instagent")
                .join("settings.json"),
            r#"{"disabledPlugins":["a"]}"#,
        )
        .unwrap();
        in_root(&user_plugins(&env), "a", "1.0.0");
        in_root(&user_plugins(&env), "b", "1.0.0");

        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&[][..]));
        let set = discover(cwd.path(), &settings, &[], &[]).unwrap();
        assert!(
            set.plugins.is_empty(),
            "白名单被高层清空后仍禁用其他名字：{:?}",
            names(&set)
        );
    }

    /// I02 消费端回归：安装根下的 `.replaced-*` 恢复副本与 `.tmp-install`
    /// staging 不被发现（包括不出现在 skipped 诊断里）；同名活插件不被
    /// 旧备份顶替。
    #[test]
    fn internal_replaced_and_staging_dirs_are_not_discovered() {
        let env = isolated_agents();
        let user = user_plugins(&env);
        in_root(&user, "alpha", "2.0.0");
        // 同名旧版本的恢复副本：排序上先于 `alpha`，若被扫描会顶替活插件。
        write_plugin(&user.join(".replaced-alpha-0001"), "alpha", "1.0.0");
        // 正在安装的 staging 副本。
        write_plugin(
            &user.join(".tmp-install").join("uuid-half"),
            "beta",
            "1.0.0",
        );

        let cwd = TempDir::new().unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert_eq!(names(&set), ["alpha"]);
        assert_eq!(set.get("alpha").unwrap().manifest.version, "2.0.0");
        assert_eq!(set.get("alpha").unwrap().root, user.join("alpha"));
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
    }

    /// 高层没写 `enabledPlugins` 时低层白名单延续（三态在消费端不回归）。
    #[test]
    fn missing_enabled_key_in_higher_layer_continues_whitelist() {
        let config = TempDir::new().unwrap();
        let env = isolated_agents();
        std::env::set_var("INSTAGENT_CONFIG_DIR", config.path());
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        )
        .unwrap();
        let cwd = TempDir::new().unwrap();
        std::fs::create_dir_all(cwd.path().join(".config").join("instagent")).unwrap();
        std::fs::write(
            cwd.path()
                .join(".config")
                .join("instagent")
                .join("settings.json"),
            r#"{"disabledPlugins":["zzz"]}"#,
        )
        .unwrap();
        in_root(&user_plugins(&env), "a", "1.0.0");
        in_root(&user_plugins(&env), "b", "1.0.0");

        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&["a".to_string()][..]));
        let set = discover(cwd.path(), &settings, &[], &[]).unwrap();
        assert_eq!(names(&set), ["a"]);
    }

    #[test]
    fn three_layer_settings_conflict_drives_discovery() {
        let config = TempDir::new().unwrap();
        let env = isolated_agents();
        std::env::set_var("INSTAGENT_CONFIG_DIR", config.path());
        let write = |dir: &Path, file: &str, content: &str| {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write(
            config.path(),
            "settings.json",
            r#"{"disabledPlugins":["a"]}"#,
        );
        let project = TempDir::new().unwrap();
        let project_settings = project.path().join(".config").join("instagent");
        write(
            &project_settings,
            "settings.json",
            r#"{"enabledPlugins":["a","b"]}"#,
        );
        write(
            &project_settings,
            "settings.local.json",
            r#"{"disabledPlugins":["b"]}"#,
        );
        let user = user_plugins(&env);
        for name in ["a", "b", "c"] {
            in_root(&user, name, "1.0.0");
        }

        // local 认领 b（disabled）；project 的 enabled [a,b] 只剩 a 生效，
        // 于是白名单 [a] 压制 user 层对 a 的 disabled。
        let settings = Settings::merged(project.path()).unwrap();
        let set = discover(project.path(), &settings, &[], &[]).unwrap();
        assert_eq!(names(&set), ["a"]);
    }

    #[test]
    fn extra_path_pointing_at_plugin_root_scans_children() {
        let env = isolated_agents();
        let root = TempDir::new().unwrap();
        in_root(root.path(), "one", "1.0.0");
        in_root(root.path(), "two", "1.0.0");
        std::fs::write(root.path().join("notes.txt"), "not a plugin").unwrap();
        let set = discover(
            env.agents.path(),
            &Settings::default(),
            &[root.path().to_path_buf()],
            &[],
        )
        .unwrap();
        assert_eq!(names(&set), ["one", "two"]);
        assert!(set.skipped.is_empty());
    }

    #[test]
    fn missing_scan_roots_are_fine() {
        let _env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert!(set.plugins.is_empty());
        assert!(set.skipped.is_empty());
    }

    /// 第三版 §5 P7"插件目录被删"：用户/项目根缺失静默，显式声明的路径
    /// （配置 `plugins` / `--plugin`）缺失 → skipped 警告、不 panic。
    #[test]
    fn missing_explicit_paths_are_warned_not_fatal() {
        let _env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let gone = cwd.path().join("deleted-plugin");
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[gone.join("from-config")],
            std::slice::from_ref(&gone),
        )
        .unwrap();
        assert!(set.plugins.is_empty());
        assert_eq!(set.skipped.len(), 2, "{:?}", set.skipped);
        for skipped in &set.skipped {
            assert!(
                skipped.reason.contains("not found"),
                "{skipped:?} 应指出路径缺失"
            );
        }
        assert!(set.skipped.iter().any(|s| s.path == gone));
        assert!(set
            .skipped
            .iter()
            .any(|s| s.path == gone.join("from-config")));
    }

    /// E8：CLI 显式路径的诊断有独立来源名，且带解析后的绝对路径（相对输入
    /// 也一样），与配置 `Extra` 路径的文案可区分。
    #[test]
    fn cli_path_diagnostics_name_source_and_resolved_path() {
        let _env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let cli = cwd.path().join("cli-plugin");
        let extra = cwd.path().join("extra-plugin");
        let set = discover(
            cwd.path(),
            &Settings::default(),
            std::slice::from_ref(&extra),
            std::slice::from_ref(&cli),
        )
        .unwrap();
        let by_path = |want: &Path| -> String {
            set.skipped
                .iter()
                .find(|s| s.path == want)
                .unwrap_or_else(|| panic!("{} 应有诊断：{:?}", want.display(), set.skipped))
                .reason
                .clone()
        };
        let cli_reason = by_path(&cli);
        let extra_reason = by_path(&extra);
        assert!(cli_reason.contains("CLI --plugin path"), "{cli_reason}");
        assert!(
            extra_reason.contains("configured plugin path"),
            "{extra_reason}"
        );
        assert!(
            !cli_reason.contains("configured plugin path"),
            "CLI 与配置路径必须可区分：{cli_reason}"
        );
        for (path, reason) in [(&cli, &cli_reason), (&extra, &extra_reason)] {
            assert!(
                reason.contains(&located(path)),
                "{reason} 应含解析后的绝对路径"
            );
        }

        // 相对路径输入也按绝对形态报出。
        let rel = PathBuf::from("relative-missing-plugin");
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[],
            std::slice::from_ref(&rel),
        )
        .unwrap();
        let expected = std::path::absolute(&rel).unwrap();
        assert!(
            set.skipped
                .iter()
                .any(|s| s.reason.contains(&expected.display().to_string())),
            "{:?}",
            set.skipped
        );
    }

    /// 显式路径存在但整条扫不到插件（空目录 / 只有散文件）也要可见；
    /// 同样形状出现在非显式层（用户根）时不算问题。
    #[test]
    fn explicit_path_without_any_plugin_is_reported() {
        let env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let empty = TempDir::new().unwrap();
        std::fs::write(empty.path().join("notes.md"), "not a plugin").unwrap();
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[empty.path().to_path_buf()],
            &[],
        )
        .unwrap();
        assert_eq!(set.skipped.len(), 1, "{:?}", set.skipped);
        assert!(set.skipped[0].reason.contains("no plugin found"));

        std::fs::create_dir_all(user_plugins(&env)).unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
    }

    /// 重复来源：同一路径既在配置里又在 `--plugin` 里 → 只注册一次，
    /// 由 CLI 层覆盖来源，不产生重复诊断。
    #[test]
    fn duplicate_explicit_sources_register_once() {
        let _env = isolated_agents();
        let cwd = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        in_root(root.path(), "dup", "1.0.0");
        let path = root.path().to_path_buf();
        let set = discover(
            cwd.path(),
            &Settings::default(),
            &[path.clone(), path.clone()],
            std::slice::from_ref(&path),
        )
        .unwrap();
        assert_eq!(names(&set), ["dup"]);
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
        assert_eq!(set.get("dup").unwrap().source, PluginSource::Cli);
    }

    /// 根目录读不了（被普通文件占住 / 权限）不静默：任何层都要诊断。
    #[test]
    fn unreadable_scan_root_is_reported() {
        let env = isolated_agents();
        std::fs::write(env.agents.path().join("plugins"), b"not a dir").unwrap();
        let cwd = TempDir::new().unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert_eq!(set.skipped.len(), 1, "{:?}", set.skipped);
        assert!(
            set.skipped[0].reason.contains("failed to read plugin root"),
            "{:?}",
            set.skipped
        );
    }

    /// R12：权限错误与坏 symlink 属"读取失败"，必须出现在诊断里；
    /// 指向真插件目录的 symlink 正常发现。
    #[cfg(unix)]
    #[test]
    fn permission_denied_and_broken_symlinks_are_diagnosed() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt as _;
        let env = isolated_agents();
        let user = user_plugins(&env);
        in_root(&user, "good", "1.0.0");
        let locked = user.join("locked");
        write_plugin(&locked, "locked", "1.0.0");
        symlink(user.join("nowhere"), user.join("dangling")).unwrap();
        symlink(user.join("good"), user.join("linked")).unwrap();
        let dangling_root = user.join("dangling-root");
        symlink(user.join("nowhere2"), &dangling_root).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let cwd = TempDir::new().unwrap();
        let set = discover(
            cwd.path(),
            &Settings::default(),
            std::slice::from_ref(&dangling_root),
            &[],
        );
        // TempDir 删除需要可写权限，先恢复。
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set = set.unwrap();

        assert_eq!(names(&set), ["good"], "指向插件目录的 symlink 应正常发现");
        let reasons: Vec<&str> = set.skipped.iter().map(|s| s.reason.as_str()).collect();
        assert!(
            set.skipped
                .iter()
                .any(|s| s.path == locked && s.reason.contains("cannot stat")),
            "无权限的插件目录必须报出来：{reasons:?}"
        );
        assert!(
            set.skipped
                .iter()
                .any(|s| s.path == user.join("dangling") && s.reason.contains("broken symlink")),
            "坏 symlink 必须报出来：{reasons:?}"
        );
        assert!(
            set.skipped
                .iter()
                .any(|s| s.path == dangling_root && s.reason.contains("broken symlink")),
            "配置路径本身是坏 symlink 时要说清：{reasons:?}"
        );
    }
}
