//! 插件发现与启用（第三版 §2.10）。
//!
//! 扫描顺序：`~/.agents/plugins/`（用户）→ `<project>/.agents/plugins/`（项目）→
//! 配置 `plugins` 额外路径（`01`）→ 运行时 `--plugin PATH`。同名插件按层覆盖：
//! Extra/CLI > 项目 > 用户（[`PluginSource`] 注释）；层内按扫描顺序先到先得。
//! 额外路径与 CLI 参数自动识别：目录本身含 `plugin.json` 视为单个插件，
//! 否则视为插件根目录逐个子目录扫描。manifest 校验失败的目录记录原因并跳过，
//! 不中断整体发现。
//!
//! 启用判定用 `01` 三层合并后的 settings（优先级 local > project > user 已在
//! 合并时体现）：`enabledPlugins` 非空即白名单模式，否则"除 `disabledPlugins`
//! 外全启用"。合并形状 `Vec<String>` 不区分"显式写空数组"与"没写"，
//! 显式空白名单因此等同于黑名单模式。

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use crate::plugin::manifest::read_manifest;
use crate::plugin::Plugin;
use crate::plugin::PluginSource;
use crate::settings::Settings;

/// 启用的插件集合；迭代得到 {manifest, root, source}，供 `06/07/10/13/15` 使用。
#[derive(Debug, Clone, Default)]
pub struct PluginSet {
    /// 按插件名升序。
    pub plugins: Vec<Plugin>,
    /// manifest 校验（`04`）失败、被发现但跳过的目录及原因。
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

/// 同名覆盖的层序（比 [`PluginSource`] 更细：配置额外路径与 CLI 参数同为
/// `Extra`，但 CLI 后扫描、优先覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    User = 0,
    Project = 1,
    Extra = 2,
    Cli = 3,
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
        PluginSource::User,
        Layer::User,
        &mut found,
        &mut skipped,
    );
    scan_path(
        &cwd.join(".agents").join("plugins"),
        PluginSource::Project,
        Layer::Project,
        &mut found,
        &mut skipped,
    );
    for path in extra_paths {
        scan_path(
            path,
            PluginSource::Extra,
            Layer::Extra,
            &mut found,
            &mut skipped,
        );
    }
    for path in cli_plugins {
        scan_path(
            path,
            PluginSource::Extra,
            Layer::Cli,
            &mut found,
            &mut skipped,
        );
    }

    let whitelist = !settings.enabled_plugins.is_empty();
    let plugins = found
        .into_values()
        .map(|candidate| candidate.plugin)
        .filter(|plugin| {
            let name = &plugin.manifest.name;
            if whitelist {
                settings.enabled_plugins.contains(name)
            } else {
                !settings.disabled_plugins.contains(name)
            }
        })
        .collect();
    Ok(PluginSet { plugins, skipped })
}

/// `path` 含 `plugin.json` 则按单个插件目录注册，否则作为插件根目录按名称
/// 升序扫描其子目录（目录内先到先得，同名靠 [`Layer`] 覆盖）。
/// 用户/项目扫描根不存在属正常（首次运行）；配置 `plugins` 与 `--plugin`
/// 是用户显式声明的路径，缺失（被删、写错）记 skipped 警告并在启动提示里
/// 给出（第三版 §5 P7"插件目录被删"），不中断发现。
fn scan_path(
    path: &Path,
    source: PluginSource,
    layer: Layer,
    found: &mut BTreeMap<String, Candidate>,
    skipped: &mut Vec<SkippedPlugin>,
) {
    if path.join("plugin.json").is_file() {
        register(path, source, layer, found, skipped);
        return;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if matches!(layer, Layer::Extra | Layer::Cli) {
                skipped.push(SkippedPlugin {
                    path: path.to_path_buf(),
                    reason: "explicitly configured plugin path not found \
                             (deleted, moved, or misspelled)"
                        .to_string(),
                });
            }
            return;
        }
        Err(err) => {
            skipped.push(SkippedPlugin {
                path: path.to_path_buf(),
                reason: format!("failed to read plugin root: {err}"),
            });
            return;
        }
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        register(&dir, source, layer, found, skipped);
    }
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
                reason: format!("{err:#}"),
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
        assert_eq!(set.get("delta").unwrap().source, PluginSource::Extra);
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
        std::fs::create_dir_all(empty).unwrap();
        // 根目录里的散文件不是插件候选，也不报错。
        std::fs::write(user.join("README.md"), "stray file").unwrap();

        let cwd = TempDir::new().unwrap();
        let set = discover(cwd.path(), &Settings::default(), &[], &[]).unwrap();
        assert_eq!(names(&set), ["good"]);
        let skipped: Vec<&Path> = set.skipped.iter().map(|s| s.path.as_path()).collect();
        assert!(skipped.contains(&&*no_schema), "{skipped:?}");
        assert!(skipped.contains(&&*broken), "{skipped:?}");
        assert!(skipped.contains(&user.join("not-a-plugin").as_path()));
        assert!(set
            .skipped
            .iter()
            .any(|s| { s.path == no_schema && s.reason.contains("must declare `$schema`") }));
        assert!(set
            .skipped
            .iter()
            .any(|s| { s.path == broken && s.reason.contains("Failed to parse") }));
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
}
