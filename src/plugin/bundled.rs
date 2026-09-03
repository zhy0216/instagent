//! bundled 插件：`include_dir` 内嵌仓库 `bundled/` 目录（第三版 §1）。
//!
//! 内核与 bundled 的关系同外部插件：同一份 `04` 校验、同一套启用判定，
//! 只是来源为编译期内嵌而非文件系统目录。加载时先把内嵌树物化到
//! `<data_dir>/bundled/`（`Plugin.root` 需要真实路径给组件运行时用），
//! 再走 [`read_manifest`]。bundled 恒为最低优先级：同名插件一律被
//! 外部发现的结果覆盖（第三版 §1"用户插件优先"）。
//! bundled provider 定义不使用 `${PLUGIN_ROOT}`（无稳定文件系统 root）。

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use include_dir::include_dir;
use include_dir::Dir;
use include_dir::DirEntry;

use crate::plugin::discovery::discover;
use crate::plugin::discovery::PluginSet;
use crate::plugin::discovery::SkippedPlugin;
use crate::plugin::install::data_dir;
use crate::plugin::manifest::read_manifest;
use crate::plugin::Plugin;
use crate::plugin::PluginSource;
use crate::settings::Settings;

/// 伪插件名：`plugin list` 与工具前缀规则统一用。
pub const BUNDLED_PLUGIN_NAME: &str = "bundled";

/// 编译期内嵌仓库根 `bundled/` 目录（`File::path()` 相对该根）。
static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/bundled");

/// 把内嵌树写到 `<base>/bundled/`（每次覆盖，保证与二进制一致），返回
/// 物化根目录。数据根目录由参数给出（同 `install.rs`，测试不改写进程
/// 全局 `INSTAGENT_DATA_DIR`，避免与 `session.rs` 测试互踩）。
pub(crate) fn materialize_at(base: &Path) -> crate::Result<PathBuf> {
    let root = base.join(BUNDLED_PLUGIN_NAME);
    write_entries(&BUNDLED, &root)?;
    Ok(root)
}

fn write_entries(dir: &Dir, root: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(root)?;
    for entry in dir.entries() {
        match entry {
            DirEntry::File(file) => {
                let target = root.join(file.path());
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, file.contents())?;
            }
            DirEntry::Dir(sub) => write_entries(sub, root)?,
        }
    }
    Ok(())
}

/// 内嵌插件物化后加载：与外部插件同一条 manifest 校验路径。
/// bundled 恒为最低优先级，同名一律被用户插件覆盖（第三版 §1）。
pub fn load_bundled() -> crate::Result<Plugin> {
    load_bundled_at(&data_dir()?)
}

/// [`load_bundled`] 的路径层：物化到给定数据根目录下（测试用，不改
/// 进程全局 `INSTAGENT_DATA_DIR`）。
pub(crate) fn load_bundled_at(base: &Path) -> crate::Result<Plugin> {
    let root = materialize_at(base)?;
    let manifest = read_manifest(&root)?;
    if manifest.name != BUNDLED_PLUGIN_NAME {
        bail!(
            "bundled plugin.json declares name `{}` (expected `{BUNDLED_PLUGIN_NAME}`)",
            manifest.name
        );
    }
    Ok(Plugin {
        manifest,
        root,
        source: PluginSource::Bundled,
    })
}

/// [`discover`] + bundled 注入：`05` 的四层不动，bundled 作为内置来源
/// 补在最低优先级（合并规则见 [`with_bundled`]）。
pub fn discover_with_bundled(
    cwd: &Path,
    settings: &Settings,
    extra_paths: &[PathBuf],
    cli_plugins: &[PathBuf],
) -> crate::Result<PluginSet> {
    let set = discover(cwd, settings, extra_paths, cli_plugins)?;
    let bundled = load_bundled()?;
    let mut set = with_bundled(set, bundled, settings);
    // 白名单显式启用的插件在所有层都找不到（目录被删、改名）：记 skipped
    // 给启动警告，不 panic（第三版 §5 P7"插件目录被删"）。manifest 已坏
    // 的目录发现层记过 skipped，不重复报。
    for name in &settings.enabled_plugins {
        let known = set.get(name).is_some()
            || set.skipped.iter().any(|skipped| {
                skipped
                    .path
                    .file_name()
                    .is_some_and(|dir| dir == name.as_str())
            });
        if !known {
            set.skipped.push(SkippedPlugin {
                path: crate::plugin::discovery::agents_dir()?
                    .join("plugins")
                    .join(name),
                reason: format!(
                    "plugin `{name}` is enabled in settings but was not found in any \
                     plugin root (directory deleted, moved, or renamed?)"
                ),
            });
        }
    }
    Ok(set)
}

/// 纯合并：外部任一层已有同名 `bundled` 时不注入；启用判定与 `05`
/// 一致（白名单模式须显式列出，黑名单模式须不在 disabled 里）。
fn with_bundled(mut set: PluginSet, bundled: Plugin, settings: &Settings) -> PluginSet {
    let name = &bundled.manifest.name;
    if set.get(name).is_some() {
        return set;
    }
    let enabled = if settings.enabled_plugins.is_empty() {
        !settings.disabled_plugins.contains(name)
    } else {
        settings.enabled_plugins.contains(name)
    };
    if enabled {
        set.plugins.push(bundled);
        set.plugins
            .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// env 隔离约定同 `discovery.rs`（lock_env 串行化进程级变量）。
    /// bundled 物化走 `*_at` 参数化入口，本就不需要设 `INSTAGENT_DATA_DIR`；
    /// `session.rs` 的测试自 `19` 起共用同一把 lock_env，不再有并行互踩。
    struct Env {
        _guard: MutexGuard<'static, ()>,
        agents: TempDir,
        data: TempDir,
    }

    fn isolated() -> Env {
        let guard = crate::config::lock_env();
        let agents = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        std::env::set_var("INSTAGENT_AGENTS_DIR", agents.path());
        // discover_with_bundled 的 `load_bundled()` 走全局 data_dir()；
        // lock_env 已全 crate 统一（`19`），覆盖它不再与 session 测试互踩。
        std::env::set_var("INSTAGENT_DATA_DIR", data.path());
        Env {
            _guard: guard,
            agents,
            data,
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

    fn names(set: &PluginSet) -> Vec<&str> {
        set.iter().map(|p| p.manifest.name.as_str()).collect()
    }

    #[test]
    fn materializes_and_loads_bundled_manifest() {
        let env = isolated();
        let plugin = load_bundled_at(env.data.path()).unwrap();
        assert_eq!(plugin.manifest.name, BUNDLED_PLUGIN_NAME);
        assert_eq!(plugin.source, PluginSource::Bundled);
        assert!(plugin.root.join("plugin.json").is_file());
        assert_eq!(
            plugin.root,
            env.data.path().join(BUNDLED_PLUGIN_NAME),
            "materialized under the given data root"
        );
    }

    #[test]
    fn bundled_providers_exist_and_avoid_plugin_root() {
        let env = isolated();
        let plugin = load_bundled_at(env.data.path()).unwrap();
        let providers = plugin.root.join(crate::plugin::NAMESPACE).join("providers");
        let entries: Vec<_> = std::fs::read_dir(&providers)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(!entries.is_empty(), "placeholder providers must exist");
        for path in &entries {
            assert!(path.extension().is_some_and(|e| e == "json"));
            let text = std::fs::read_to_string(path).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(parsed["name"].is_string(), "{path:?}");
            assert_eq!(parsed["engine"], "openai", "{path:?}");
            assert!(
                !text.contains("${PLUGIN_ROOT}"),
                "bundled providers must not use ${{PLUGIN_ROOT}}: {path:?}"
            );
        }
    }

    #[test]
    fn bundled_appears_in_plugin_set() {
        let env = isolated();
        let set = discover(env.agents.path(), &Settings::default(), &[], &[]).unwrap();
        assert!(set.plugins.is_empty(), "fresh env has no external plugin");
        let bundled = load_bundled_at(env.data.path()).unwrap();
        let set = with_bundled(set, bundled, &Settings::default());
        let plugin = set.get(BUNDLED_PLUGIN_NAME).expect("bundled in set");
        assert_eq!(plugin.source, PluginSource::Bundled);
    }

    /// 第三版 §5 P7"启用的插件目录被删"：白名单里的插件在所有扫描层都
    /// 找不到 → skipped 警告（启动时经 notes 展示），不 panic。
    #[test]
    fn enabled_plugin_with_deleted_dir_warns_without_panic() {
        let env = isolated();
        let dir = env.agents.path().join("plugins").join("gone");
        write_plugin(&dir, "gone", "1.0.0");
        std::fs::remove_dir_all(&dir).unwrap();
        let settings = Settings {
            enabled_plugins: vec!["gone".into()],
            ..Settings::default()
        };
        let set = discover_with_bundled(env.agents.path(), &settings, &[], &[]).unwrap();
        assert!(set.plugins.is_empty());
        let skipped = set
            .skipped
            .iter()
            .find(|s| s.reason.contains("`gone`"))
            .expect("被删的启用插件必须有可读警告");
        assert!(
            skipped.reason.contains("not found"),
            "{skipped:?} 应说明找不到目录"
        );
        assert!(skipped.path.ends_with("plugins/gone"), "{skipped:?}");
    }

    /// 能找到的启用插件（含 bundled）不误报；坏 manifest 已被发现层记
    /// skipped，不重复报"找不到"。
    #[test]
    fn existing_enabled_plugins_are_not_warned() {
        let env = isolated();
        write_plugin(
            &env.agents.path().join("plugins").join("kept"),
            "kept",
            "1.0.0",
        );
        let settings = Settings {
            enabled_plugins: vec!["kept".into(), BUNDLED_PLUGIN_NAME.into()],
            ..Settings::default()
        };
        let set = discover_with_bundled(env.agents.path(), &settings, &[], &[]).unwrap();
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);
        assert_eq!(names(&set), vec![BUNDLED_PLUGIN_NAME, "kept"]);
    }

    #[test]
    fn bundled_respects_settings_and_yields_to_user_plugin() {
        let env = isolated();
        let merge = |settings: &Settings| -> PluginSet {
            let set = discover(env.agents.path(), settings, &[], &[]).unwrap();
            let bundled = load_bundled_at(env.data.path()).unwrap();
            with_bundled(set, bundled, settings)
        };

        // 黑名单：disabledPlugins 可关掉 bundled。
        let settings = Settings {
            disabled_plugins: vec![BUNDLED_PLUGIN_NAME.into()],
            ..Settings::default()
        };
        assert!(merge(&settings).get(BUNDLED_PLUGIN_NAME).is_none());

        // 白名单：没列出 bundled 即不启用；列出则在。
        let settings = Settings {
            enabled_plugins: vec!["other".into()],
            ..Settings::default()
        };
        assert!(merge(&settings).get(BUNDLED_PLUGIN_NAME).is_none());
        let settings = Settings {
            enabled_plugins: vec![BUNDLED_PLUGIN_NAME.into()],
            ..Settings::default()
        };
        assert_eq!(names(&merge(&settings)), vec![BUNDLED_PLUGIN_NAME]);

        // 同名覆盖：用户装了名为 bundled 的插件时，bundled 不注入。
        write_plugin(
            &env.agents.path().join("plugins").join(BUNDLED_PLUGIN_NAME),
            BUNDLED_PLUGIN_NAME,
            "9.9.9",
        );
        let plugin = merge(&Settings::default())
            .get(BUNDLED_PLUGIN_NAME)
            .unwrap()
            .clone();
        assert_eq!(plugin.source, PluginSource::User);
        assert_eq!(plugin.manifest.version, "9.9.9");
    }
}
