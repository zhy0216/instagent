//! 三层 settings（第三版 §2.10，goose 草案字段名）。
//!
//! 读取并合并 `~/.config/instagent/settings.json`（User）→
//! `<project>/.config/instagent/settings.json`（Project）→
//! 同目录 `settings.local.json`（Local）。优先级 local > project > user：
//! 同名插件以最高层出现的字段为准（enabled/disabled 互斥覆盖；trusted
//! 独立取并集）。启用判定逻辑在 `05`。

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// settings 文件来源层，优先级 Local > Project > User。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsLayer {
    /// `~/.config/instagent/settings.json`
    User,
    /// `<project>/.config/instagent/settings.json`
    Project,
    /// 同目录 `settings.local.json`
    Local,
}

/// 三层合并后的 settings 形状（字段名按规范为 camelCase）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 写了即白名单模式；没写则"除 `disabled_plugins` 外全启用"。
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
    /// 首次启用确认的结果（第三版 §2.10 信任），CLI 接线在 `18`。
    pub trusted_plugins: Vec<String>,
}

/// 单个文件里的形状：区分"字段缺失"（None）与"写了空数组"（Some(vec![])），
/// 缺失字段不参与对下层的覆盖。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LayerFile {
    enabled_plugins: Option<Vec<String>>,
    disabled_plugins: Option<Vec<String>>,
    trusted_plugins: Option<Vec<String>>,
}

impl Settings {
    fn user_path() -> crate::Result<PathBuf> {
        Ok(crate::config::config_dir()?.join("settings.json"))
    }

    /// 读用户层 settings（文件不存在 = 默认值）。
    pub fn load_user() -> crate::Result<Settings> {
        Ok(match std::fs::read_to_string(Self::user_path()?) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        })
    }

    /// 写回用户层。
    pub fn save_user(&self) -> crate::Result<()> {
        let path = Self::user_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// 读三层文件并合并（local > project > user）。缺文件按该层无内容处理。
    pub fn merged(cwd: &Path) -> crate::Result<Settings> {
        let user = read_layer(&Self::user_path()?)?;
        let project = read_layer(&project_settings_path(cwd, SettingsLayer::Project))?;
        let local = read_layer(&project_settings_path(cwd, SettingsLayer::Local))?;
        Ok(merge_layers([user, project, local]))
    }

    /// 写回指定层（`18` 的 enable/disable 与信任确认用）。
    pub fn save(&self, cwd: &Path, layer: SettingsLayer) -> crate::Result<()> {
        let path = match layer {
            SettingsLayer::User => crate::config::config_dir()?.join("settings.json"),
            SettingsLayer::Project | SettingsLayer::Local => project_settings_path(cwd, layer),
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// `<project>/.config/instagent/settings.json`（Local 层为 `settings.local.json`）。
fn project_settings_path(cwd: &Path, layer: SettingsLayer) -> PathBuf {
    let file = if layer == SettingsLayer::Local {
        "settings.local.json"
    } else {
        "settings.json"
    };
    cwd.join(".config").join("instagent").join(file)
}

fn read_layer(path: &Path) -> crate::Result<Option<LayerFile>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `layers` 按 [User, Project, Local] 排列；从最高层向最低层应用，
/// 被高层提到过的名字（enabled/disabled 任一）不再被低层覆盖。
fn merge_layers(layers: [Option<LayerFile>; 3]) -> Settings {
    let mut merged = Settings::default();
    let mut claimed: HashSet<String> = HashSet::new();
    for file in layers.iter().rev() {
        let Some(file) = file else { continue };
        let enabled = file.enabled_plugins.clone().unwrap_or_default();
        let disabled = file.disabled_plugins.clone().unwrap_or_default();
        push_unclaimed(&mut merged.enabled_plugins, &enabled, &claimed);
        push_unclaimed(&mut merged.disabled_plugins, &disabled, &claimed);
        claimed.extend(enabled.into_iter().chain(disabled));
        if let Some(trusted) = &file.trusted_plugins {
            push_new(&mut merged.trusted_plugins, trusted);
        }
    }
    merged
}

fn push_unclaimed(dest: &mut Vec<String>, names: &[String], claimed: &HashSet<String>) {
    for name in names {
        if !claimed.contains(name.as_str()) && !dest.contains(name) {
            dest.push(name.clone());
        }
    }
}

fn push_new(dest: &mut Vec<String>, names: &[String]) {
    for name in names {
        if !dest.contains(name) {
            dest.push(name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_user_dir() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let guard = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("INSTAGENT_CONFIG_DIR", dir.path());
        (guard, dir)
    }

    fn write_json(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn missing_files_fall_back_to_defaults() {
        let (_guard, _user) = temp_user_dir();
        let cwd = tempfile::tempdir().unwrap();
        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn reads_camel_case_fields_from_all_layers() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["u"],"trustedPlugins":["t-u"]}"#,
        );
        write_json(
            &project_settings_path(project.path(), SettingsLayer::Project),
            r#"{"enabledPlugins":["p"]}"#,
        );
        write_json(
            &project_settings_path(project.path(), SettingsLayer::Local),
            r#"{"disabledPlugins":["l"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.enabled_plugins, vec!["p", "u"]);
        assert_eq!(settings.disabled_plugins, vec!["l"]);
        assert_eq!(settings.trusted_plugins, vec!["t-u"]);
    }

    #[test]
    fn higher_layer_claim_overrides_lower_field() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["shared","user-only"],"disabledPlugins":["d-user"]}"#,
        );
        write_json(
            &project_settings_path(project.path(), SettingsLayer::Project),
            r#"{"disabledPlugins":["shared"]}"#,
        );
        write_json(
            &project_settings_path(project.path(), SettingsLayer::Local),
            r#"{"enabledPlugins":["shared","local-only"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        // local 最高：shared 归 enabled；project 的 disabled [shared] 被压掉。
        assert_eq!(
            settings.enabled_plugins,
            vec!["shared", "local-only", "user-only"]
        );
        assert_eq!(settings.disabled_plugins, vec!["d-user"]);
    }

    #[test]
    fn trusted_plugins_merge_independently() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"trustedPlugins":["a","b"]}"#,
        );
        write_json(
            &project_settings_path(project.path(), SettingsLayer::Project),
            r#"{"disabledPlugins":["a"],"trustedPlugins":["a","c"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        // "a" 被 project 的 disabled 认领，不影响 user 层的 trusted 记录。
        assert_eq!(settings.disabled_plugins, vec!["a"]);
        assert_eq!(settings.trusted_plugins, vec!["a", "c", "b"]);
    }

    #[test]
    fn save_round_trips_per_layer() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        let settings = Settings {
            enabled_plugins: vec!["x".into()],
            disabled_plugins: vec!["y".into()],
            trusted_plugins: vec!["x".into()],
        };
        for layer in [
            SettingsLayer::User,
            SettingsLayer::Project,
            SettingsLayer::Local,
        ] {
            settings.save(project.path(), layer).unwrap();
        }
        assert_eq!(
            read_json(user.path().join("settings.json")),
            read_json(project_settings_path(project.path(), SettingsLayer::Local))
        );
        assert_eq!(Settings::merged(project.path()).unwrap(), settings);
    }

    fn read_json(path: PathBuf) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
}
