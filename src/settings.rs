//! 三层 settings（第三版 §2.10，goose 草案字段名）。
//!
//! 读取并合并 `~/.config/instagent/settings.json`（User）→
//! `<project>/.config/instagent/settings.json`（Project）→
//! 同目录 `settings.local.json`（Local）。优先级 local > project > user：
//! 同名插件以最高层出现的字段为准（enabled/disabled 互斥覆盖）。
//! `enabledPlugins` 三态语义按 ADR 0003 D5：缺失 = 不表态、非空 = 白名单、
//! `[]` = 显式空白名单终值（低层不得恢复任何 enabled 名字）。合并、serde
//! 往返与 enable/disable 全用同一判定：区分"未声明白名单"与"声明后剩余集合
//! 为空"——后者仍是白名单模式（禁用全部），绝不退化成"全启用"（I01）。
//! `disabledPlugins` 维持并集，缺失与 `[]` 等价。
//! 单层读取有字节预算（[`SETTINGS_FILE_MAX_BYTES`]）；超限 / 坏 JSON / IO
//! 错误都带该层文件路径且不回显内容，原文件保持不变；只有 `NotFound` 按
//! 该层无内容处理。所有写路径走 `write_private_atomic`（同目录临时文件 +
//! fsync + rename，Unix mode 0600），崩溃或 rename 失败只会留下旧的完整
//! 文件或新的完整文件。

use std::collections::HashSet;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// 单个 settings 文件的读取字节预算（方案默认：config/settings 1 MiB）。
/// 超限直接报错并保留原文件，不做截断读取。
const SETTINGS_FILE_MAX_BYTES: u64 = 1024 * 1024;

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
/// Serialize/Deserialize 均为手写，保证"未表态 / 白名单 / 显式空白名单"三态
/// 在直接 serde 往返与 load/save 中都不漂移（I01）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// 写了即白名单模式；没写则"除 `disabled_plugins` 外全启用"。
    /// 层内的三态（缺失 / `[]` / 非空，ADR 0003 D5）由 `merge_layers` 处理。
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
    /// 合并终值是"白名单模式且剩余集合为空"（ADR 0003 D5）：某层显式写了
    /// `[]`（终值，低层不得恢复名字），或低层声明的白名单被高层全部认领。
    /// 两种情况下 `enabled_plugins` 为空但白名单模式仍成立，即禁用全部。
    /// 消费端判模式用 [`Settings::whitelist`]，不要拿
    /// `enabled_plugins.is_empty()` 猜——那样显式 `[]` 会退化成"全启用"。
    /// 序列化形态由手写 Serialize/Deserialize 决定：本字段不落盘，随
    /// `enabledPlugins` 键的缺失 / `[]` 形态往返。
    pub enabled_locked: bool,
}

/// 手写序列化（与手写 [`Deserialize`] 对称）：未表态（`enabled_locked ==
/// false`）且 `enabled_plugins` 为空时写成缺失键而不是 `[]`——缺失键保留
/// "不表态"（黑名单模式），`[]` 保留给"显式空白名单"（禁用全部）。
impl Serialize for Settings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(2))?;
        if !self.enabled_plugins.is_empty() || self.enabled_locked {
            map.serialize_entry("enabledPlugins", &self.enabled_plugins)?;
        }
        map.serialize_entry("disabledPlugins", &self.disabled_plugins)?;
        map.end()
    }
}

/// 手写反序列化（与手写 [`Serialize`] 对称）：`enabledPlugins` 键缺失 = 不
/// 表态；出现（含 `[]`）= 白名单模式，其中 `[]` 记 `enabled_locked` 表达
/// "显式空白名单终值"。derive Deserialize 会把 `[]` 读回成不表态，
/// "禁用全部"将在下次读回时漂移成"全启用"（I01）。
impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let file = LayerFile::deserialize(deserializer)?;
        let enabled_locked = file
            .enabled_plugins
            .as_deref()
            .is_some_and(|names| names.is_empty());
        Ok(Settings {
            enabled_plugins: file.enabled_plugins.unwrap_or_default(),
            disabled_plugins: file.disabled_plugins.unwrap_or_default(),
            enabled_locked,
        })
    }
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

    /// 读用户层 settings（文件不存在 = 默认值）。走 `LayerFile` + 合并，
    /// 单层也能表达 `enabledPlugins` 的三态（ADR 0003 D5）。
    pub fn load_user() -> crate::Result<Settings> {
        Ok(merge_layers([read_layer(&Self::user_path()?)?, None, None]))
    }

    /// `enabledPlugins` 白名单视图（ADR 0003 D5 三态）：
    /// `Some(names)` = 白名单模式（含显式 `[]` = 禁用全部）；
    /// `None` = 该键从未表态，走 `disabled_plugins` 黑名单。
    pub fn whitelist(&self) -> Option<&[String]> {
        (!self.enabled_plugins.is_empty() || self.enabled_locked).then_some(&self.enabled_plugins)
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

/// 读一层 settings 文件。只有 `NotFound` 按该层无内容处理（`None`）；
/// IO 错误、超过 [`SETTINGS_FILE_MAX_BYTES`] 预算、非 UTF-8 与坏 JSON 都带
/// 该层文件路径报出，且不回显文件内容（可能含密钥等敏感字段），原文件
/// 保持不变（只读，从不截断或改写）。
fn read_layer(path: &Path) -> crate::Result<Option<LayerFile>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("read settings layer {}", path.display()))
        }
    };
    let mut buf = Vec::new();
    std::io::Read::take(&mut file, SETTINGS_FILE_MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read settings layer {}", path.display()))?;
    if buf.len() as u64 > SETTINGS_FILE_MAX_BYTES {
        bail!(
            "settings layer {} exceeds the {SETTINGS_FILE_MAX_BYTES}-byte read budget; \
             the file was left unchanged, trim it and retry",
            path.display()
        );
    }
    let text = String::from_utf8(buf).map_err(|_| {
        anyhow::anyhow!(
            "settings layer {} is not valid UTF-8; the file was left unchanged",
            path.display()
        )
    })?;
    serde_json::from_str(&text)
        .with_context(|| format!("parse settings layer {}", path.display()))
        .map(Some)
}

/// `layers` 按 [User, Project, Local] 排列；从最高层向最低层应用，
/// 被高层提到过的名字（enabled/disabled 任一）不再被低层覆盖。
/// `enabledPlugins` 三态（ADR 0003 D5）：`None` = 不表态，低层值延续；
/// 非空 = 白名单；`Some([])` = 终值，此后低层不得再恢复任何 enabled 名字
/// （高层已认领的 enabled 名字不受影响）。
/// 白名单模式在合并后保留（I01）：只要某层声明过 `enabledPlugins`，即使
/// 终值集合为空（显式 `[]`，或低层白名单被高层 disabled 全部认领），结果
/// 仍是"白名单模式 + 空集合"（禁用全部），不退化成黑名单让未提及的插件
/// 复活。消费端凭 [`Settings::enabled_locked`] 区分"没写"与"空集合"。
/// `disabledPlugins` 维持并集，`None` 与 `Some([])` 等价。
fn merge_layers(layers: [Option<LayerFile>; 3]) -> Settings {
    let mut merged = Settings::default();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut enabled_declared = false;
    let mut enabled_locked = false;
    for file in layers.iter().rev() {
        let Some(file) = file else { continue };
        match file.enabled_plugins.as_deref() {
            Some([]) => {
                enabled_declared = true;
                enabled_locked = true;
            }
            Some(names) => {
                enabled_declared = true;
                if !enabled_locked {
                    push_unclaimed(&mut merged.enabled_plugins, names, &claimed);
                    claimed.extend(names.iter().cloned());
                }
            }
            None => {}
        }
        if let Some(names) = file.disabled_plugins.as_deref() {
            push_unclaimed(&mut merged.disabled_plugins, names, &claimed);
            claimed.extend(names.iter().cloned());
        }
    }
    // 白名单模式只要某层表过态就成立：终值集合非空时模式自明；终值集合为
    // 空时靠该标志把"禁用全部"与"从未表态（全启用）"区分开。
    merged.enabled_locked = enabled_declared && merged.enabled_plugins.is_empty();
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
            ..Settings::default()
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

    /// 消费端判模式的唯一入口：三态在合并结果里都可表达。
    #[test]
    fn whitelist_accessor_exposes_all_three_states() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        // 缺失 = 不表态（None，走黑名单）。
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.whitelist(), None);
        // 非空 = 白名单。
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&["a".to_string()][..]));
        // 显式 [] = 白名单且为空 = 禁用全部。
        layer_file(
            SettingsLayer::Local,
            project.path(),
            r#"{"enabledPlugins":[]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&[][..]));
        assert!(settings.enabled_locked);
    }

    /// 写回忠实表达三态：没表态的空白名单不写键（否则 `plugin disable` 清空
    /// 白名单后会被读成"禁用全部"），显式 `[]` 终值才写空数组。
    #[test]
    fn saving_preserves_enabled_tri_state() {
        let (_guard, user) = temp_user_dir();
        let settings = Settings {
            disabled_plugins: vec!["a".into()],
            ..Settings::default()
        };
        settings.save_user().unwrap();
        assert_eq!(
            read_json(user.path().join("settings.json"))["enabledPlugins"],
            serde_json::Value::Null
        );
        assert_eq!(Settings::load_user().unwrap().whitelist(), None);

        let settings = Settings {
            enabled_locked: true,
            ..Settings::default()
        };
        settings.save_user().unwrap();
        assert_eq!(
            read_json(user.path().join("settings.json"))["enabledPlugins"],
            serde_json::json!([])
        );
        assert_eq!(Settings::load_user().unwrap().whitelist(), Some(&[][..]));
    }

    /// I01：低层白名单被高层 disabled 全部认领后，终值仍是白名单模式
    /// （空集合 = 禁用全部），未提及的名字不因翻转成黑名单而复活。
    #[test]
    fn whitelist_mode_survives_when_higher_disabled_claims_all_names() {
        let (_guard, user) = temp_user_dir();
        let project = tempfile::tempdir().unwrap();
        write_json(
            &user.path().join("settings.json"),
            r#"{"enabledPlugins":["a"]}"#,
        );
        layer_file(
            SettingsLayer::Project,
            project.path(),
            r#"{"disabledPlugins":["a"]}"#,
        );
        let settings = Settings::merged(project.path()).unwrap();
        assert_eq!(settings.whitelist(), Some(&[][..]));
        assert!(settings.enabled_locked);
        assert_eq!(settings.disabled_plugins, vec!["a".to_string()]);
    }

    /// I01：三态在直接 serde 往返中不漂移——缺失键 = 不表态，`[]` = 显式
    /// 空白名单，非空 = 白名单；写回读回原样。
    #[test]
    fn serde_round_trip_preserves_enabled_tri_state() {
        // 缺失键 = 不表态；序列化后不漂移成 `[]`。
        let settings: Settings = serde_json::from_str(r#"{"disabledPlugins":["d"]}"#).unwrap();
        assert_eq!(settings.whitelist(), None);
        let value = serde_json::to_value(&settings).unwrap();
        assert!(value.get("enabledPlugins").is_none(), "{value}");

        // 显式 [] = 禁用全部终值；往返后仍是 `[]` 而非缺失键。
        let settings: Settings = serde_json::from_str(r#"{"enabledPlugins":[]}"#).unwrap();
        assert_eq!(settings.whitelist(), Some(&[][..]));
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["enabledPlugins"], serde_json::json!([]), "{value}");
        let back: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(back, settings);

        // 非空 = 白名单；往返后不变。
        let settings: Settings = serde_json::from_str(r#"{"enabledPlugins":["a"]}"#).unwrap();
        assert_eq!(settings.whitelist(), Some(&["a".to_string()][..]));
        let back: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back, settings);
    }

    /// T2：user/project/local 任一层超出读取预算都指向对应层的文件，
    /// 原文件保持不变；恰好等于预算的文件照常解析（上限前后都要测）。
    #[test]
    fn oversized_settings_layer_is_rejected_with_its_path() {
        // `{"disabledPlugins":[""]}` 的固定开销是 24 字节，据此补齐到预算 +/-。
        let pad = |extra: usize| {
            format!(
                r#"{{"disabledPlugins":["{}"]}}"#,
                "a".repeat(SETTINGS_FILE_MAX_BYTES as usize - 24 + extra)
            )
        };
        let content_over = pad(1);
        assert_eq!(content_over.len() as u64, SETTINGS_FILE_MAX_BYTES + 1);
        for layer in [
            SettingsLayer::User,
            SettingsLayer::Project,
            SettingsLayer::Local,
        ] {
            let (_guard, user) = temp_user_dir();
            let cwd = tempfile::tempdir().unwrap();
            let path = match layer {
                SettingsLayer::User => user.path().join("settings.json"),
                other => project_settings_path(cwd.path(), other),
            };
            write_json(&path, &content_over);
            let err = Settings::merged(cwd.path()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&path.display().to_string()),
                "{layer:?} 超限错误应指向 {}: {message}",
                path.display()
            );
            assert!(message.contains("budget"), "{message}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content_over);
        }

        // 预算本身可取到：恰好等于上限的文件解析成功。
        let (_guard, user) = temp_user_dir();
        let cwd = tempfile::tempdir().unwrap();
        let content_exact = pad(0);
        assert_eq!(content_exact.len() as u64, SETTINGS_FILE_MAX_BYTES);
        write_json(&user.path().join("settings.json"), &content_exact);
        let settings = Settings::merged(cwd.path()).unwrap();
        assert_eq!(settings.disabled_plugins.len(), 1);
        assert_eq!(
            settings.disabled_plugins[0].len() as u64,
            SETTINGS_FILE_MAX_BYTES - 24
        );
    }

    /// T2：任一层坏 JSON 都指向对应层的文件，原文件保持不变。
    #[test]
    fn broken_json_points_at_the_layer_file() {
        for layer in [
            SettingsLayer::User,
            SettingsLayer::Project,
            SettingsLayer::Local,
        ] {
            let (_guard, user) = temp_user_dir();
            let cwd = tempfile::tempdir().unwrap();
            let path = match layer {
                SettingsLayer::User => user.path().join("settings.json"),
                other => project_settings_path(cwd.path(), other),
            };
            write_json(&path, "{ not json");
            let err = Settings::merged(cwd.path()).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("parse settings layer"), "{message}");
            assert!(
                message.contains(&path.display().to_string()),
                "{layer:?} 坏 JSON 错误应指向 {}: {message}",
                path.display()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        }
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
            ..Settings::default()
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
