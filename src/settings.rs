//! 三层 settings（第三版 §2.10，goose 草案字段名）。
//!
//! 读取并合并 `~/.config/instagent/settings.json`（User）→
//! `<project>/.config/instagent/settings.json`（Project）→
//! 同目录 `settings.local.json`（Local）。优先级 local > project > user：
//! 同名插件以最高层出现的字段为准（enabled/disabled 互斥覆盖）。
//! `enabledPlugins` 三态语义按 ADR 0003 D5：缺失 = 不表态、非空 = 白名单、
//! `[]` = 显式空白名单终值（低层不得恢复任何 enabled 名字）；
//! `disabledPlugins` 维持并集，缺失与 `[]` 等价。
//! 所有写路径走 [`write_private_atomic`]（同目录临时文件 + fsync + rename，
//! Unix mode 0600），崩溃或 rename 失败只会留下旧的完整文件或新的完整文件。

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

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
    /// 层内的三态（缺失 / `[]` / 非空，ADR 0003 D5）由 `merge_layers` 处理。
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
}

/// 单个文件里的形状：区分"字段缺失"（None）与"写了空数组"（Some(vec![])），
/// 缺失字段不参与对下层的覆盖。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LayerFile {
    enabled_plugins: Option<Vec<String>>,
    disabled_plugins: Option<Vec<String>>,
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
        write_layer(&Self::user_path()?, self)
    }

    /// 读三层文件并合并（local > project > user）。缺文件按该层无内容处理。
    pub fn merged(cwd: &Path) -> crate::Result<Settings> {
        let user = read_layer(&Self::user_path()?)?;
        let project = read_layer(&project_settings_path(cwd, SettingsLayer::Project))?;
        let local = read_layer(&project_settings_path(cwd, SettingsLayer::Local))?;
        Ok(merge_layers([user, project, local]))
    }

    /// 写回指定层（`18` 的 enable/disable 用）。
    pub fn save(&self, cwd: &Path, layer: SettingsLayer) -> crate::Result<()> {
        let path = match layer {
            SettingsLayer::User => crate::config::config_dir()?.join("settings.json"),
            SettingsLayer::Project | SettingsLayer::Local => project_settings_path(cwd, layer),
        };
        write_layer(&path, self)
    }
}

/// 建父目录后原子私有写入一层 settings。
fn write_layer(path: &Path, settings: &Settings) -> crate::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private_atomic(path, &serde_json::to_string_pretty(settings)?)
}

/// 原子私有写入：同目录临时文件（0600）→ write/flush/sync_all → rename →
/// 目录 sync（让 rename 本身落盘）。任何一步失败都清掉临时文件，原文件
/// 要么是旧的完整版本、要么不存在，绝不会是半份文件。
pub(crate) fn write_private_atomic(path: &Path, content: &str) -> crate::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no file name in {}", path.display()),
        )
    })?;
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));
    match write_tmp_then_rename(&tmp, path, content) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn write_tmp_then_rename(tmp: &Path, path: &Path, content: &str) -> crate::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)?;
    #[cfg(unix)]
    if let Some(parent) = tmp.parent() {
        if let Ok(dir_file) = std::fs::File::open(parent) {
            let _ = dir_file.sync_all();
        }
    }
    Ok(())
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
/// `enabledPlugins` 三态（ADR 0003 D5）：`None` = 不表态，低层值延续；
/// 非空 = 白名单；`Some([])` = 终值，此后低层不得再恢复任何 enabled 名字
/// （高层已认领的 enabled 名字不受影响）。`disabledPlugins` 维持并集，
/// `None` 与 `Some([])` 等价。
fn merge_layers(layers: [Option<LayerFile>; 3]) -> Settings {
    let mut merged = Settings::default();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut enabled_locked = false;
    for file in layers.iter().rev() {
        let Some(file) = file else { continue };
        match file.enabled_plugins.as_deref() {
            Some([]) => enabled_locked = true,
            Some(names) if !enabled_locked => {
                push_unclaimed(&mut merged.enabled_plugins, names, &claimed);
                claimed.extend(names.iter().cloned());
            }
            _ => {}
        }
        if let Some(names) = file.disabled_plugins.as_deref() {
            push_unclaimed(&mut merged.disabled_plugins, names, &claimed);
            claimed.extend(names.iter().cloned());
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
            r#"{"enabledPlugins":["u"]}"#,
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
    fn save_round_trips_per_layer() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        let settings = Settings {
            enabled_plugins: vec!["x".into()],
            disabled_plugins: vec!["y".into()],
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

    fn layer_file(layer: SettingsLayer, cwd: &Path, content: &str) {
        write_json(&project_settings_path(cwd, layer), content);
    }

    #[test]
    fn explicit_empty_enabled_is_terminal_over_lower_layers() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a","b"]}"#,
        );
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"enabledPlugins":[]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        // D5：[] = 显式空白名单，低层的 a、b 不得被恢复。
        assert!(settings.enabled_plugins.is_empty());
    }

    #[test]
    fn higher_enabled_claim_survives_lower_empty_whitelist() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        );
        layer_file(
            SettingsLayer::Project,
            project.path(),
            r#"{"enabledPlugins":[]}"#,
        );
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"enabledPlugins":["b"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        // local 认领的 b 保留；project 的 [] 只挡住更低的 user a。
        assert_eq!(settings.enabled_plugins, vec!["b".to_string()]);
    }

    #[test]
    fn missing_enabled_continues_lower_layers() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        );
        layer_file(
            SettingsLayer::Project,
            project.path(),
            r#"{"disabledPlugins":["z"]}"#,
        );
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"disabledPlugins":["a"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        // 缺失 = 不表态：a 的 enabled 由 local 认领后压掉，但延续到 user 层。
        assert!(settings.enabled_plugins.is_empty());
        assert_eq!(
            settings.disabled_plugins,
            vec!["a".to_string(), "z".to_string()]
        );
    }

    #[test]
    fn empty_disabled_is_equivalent_to_missing() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        );
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"disabledPlugins":[]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.enabled_plugins, vec!["a".to_string()]);
        assert!(settings.disabled_plugins.is_empty());
    }

    #[test]
    fn terminal_empty_with_higher_disabled_union() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"],"disabledPlugins":["u"]}"#,
        );
        layer_file(
            SettingsLayer::Project,
            project.path(),
            r#"{"enabledPlugins":[]}"#,
        );
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"disabledPlugins":["a"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert!(settings.enabled_plugins.is_empty());
        assert_eq!(
            settings.disabled_plugins,
            vec!["a".to_string(), "u".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn saves_write_private_mode_and_leave_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        let settings = Settings {
            enabled_plugins: vec!["x".into()],
            disabled_plugins: vec!["y".into()],
        };
        settings.save_user().unwrap();
        for layer in [SettingsLayer::Project, SettingsLayer::Local] {
            settings.save(project.path(), layer).unwrap();
        }
        let paths = [
            user.path().join("settings.json"),
            project_settings_path(project.path(), SettingsLayer::Project),
            project_settings_path(project.path(), SettingsLayer::Local),
        ];
        for path in &paths {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "private mode for {}", path.display());
        }
        assert_eq!(std::fs::read_dir(user.path()).unwrap().count(), 1);
        let layer_dir = project_settings_path(project.path(), SettingsLayer::Local)
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(std::fs::read_dir(layer_dir).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_dir_fails_without_touching_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_private_atomic(&path, r#"{"enabledPlugins":["a"]}"#).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let err = write_private_atomic(&path, r#"{"enabledPlugins":["b"]}"#).unwrap_err();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err.to_string().contains("Permission"), "{err}");
        // 旧文件保持完整，且没有遗留临时文件。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"enabledPlugins":["a"]}"#
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rename_failure_cleans_up_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // 目标位置被目录占住：write 成功、rename 必失败。
        std::fs::create_dir(dir.path().join("settings.json")).unwrap();
        let err = write_private_atomic(&dir.path().join("settings.json"), "{}").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("directory"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
