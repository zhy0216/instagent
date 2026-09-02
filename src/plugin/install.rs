//! 插件安装 / 更新（第三版 §2.10；逻辑参考 goose `plugins/mod.rs`，只读）。
//!
//! `install`：git-url 用 `03` 的 `git_command` 子进程 clone（不引 libgit2），
//! 本地路径直接复制 → `04` 校验 `plugin.json` → 写 `.install.json`
//! （source、commit、时间）→ 放入 `~/.agents/plugins/<name>/`。staging +
//! rename 替换，旧目录在替换成功前不动。
//! `update`：按 `.install.json` 重新拉取；`commit` 为 `None` 即本地路径
//! 安装，无法 update（goose 同款 `source_type != "git"` 拒绝，InstallInfo
//! 字段由 `00` 锁定，不另存 source_type）。自动更新 24h 节流：
//! [`auto_update_all`] 只处理 `auto_update` 且距 [`should_auto_update`]
//! 到期的插件，失败也先记检查时间，避免每次启动重试（goose 同款）。
//! list / show / enable / disable 为数据层，CLI 接线在 `18`。
//! `PLUGIN_DATA`：[`plugin_data_dir`] = `<data_dir>/plugins/<name>/`，按需创建。

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::plugin::discovery::agents_dir;
use crate::plugin::manifest::read_manifest;
use crate::plugin::manifest::validate_plugin_name;
use crate::plugin::manifest::PluginManifest;
use crate::plugin::Plugin;
use crate::plugin::PluginSource;
use crate::settings::Settings;
use crate::subprocess::git_command;

/// `.install.json` 文件名（goose `.goose-plugin-install.json` 的对应物）。
pub const INSTALL_METADATA: &str = ".install.json";

/// 24h 自动更新节流间隔（goose `AUTO_UPDATE_INTERVAL_HOURS`）。
pub const AUTO_UPDATE_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// 安装来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    GitUrl(String),
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    pub auto_update: bool,
}

/// `.install.json` 的内容（goose `.goose-plugin-install.json` 的对应物）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub source: String,
    pub commit: Option<String>,
    /// epoch 秒。
    pub installed_at: i64,
    /// 24h 自动更新节流（goose 同款）用。
    pub last_update_check: Option<i64>,
    pub auto_update: bool,
}

/// 已安装插件的清单条目（list / show 数据层）。
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub plugin: Plugin,
    /// `None` = 手动放进目录、不经 `install` 写入的插件。
    pub install_info: Option<InstallInfo>,
    /// 当前（cwd 三层合并后）settings 下的启用状态。
    pub enabled: bool,
}

/// [`auto_update_all`] 的单项结果。
#[derive(Debug)]
pub struct AutoUpdateResult {
    pub name: String,
    pub result: crate::Result<()>,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `<data_dir>`：`INSTAGENT_DATA_DIR` 优先，否则 etcetera（XDG）的
/// `~/.local/share/instagent`——约定与 `session.rs` 完全一致。
///
/// 测试优先走参数化入口而不改写进程全局 `INSTAGENT_DATA_DIR`（`session.rs`
/// 的测试用它；全 crate 测试自 `19` 起共用 `config::lock_env` 一把锁）：
/// 覆盖语义在纯函数 [`data_dir_from`] 验证，
/// 路径逻辑在 [`plugin_data_dir_at`] 验证，这里只是取环境变量的胶水。
pub(crate) fn data_dir() -> crate::Result<PathBuf> {
    data_dir_from(std::env::var_os("INSTAGENT_DATA_DIR"))
}

fn data_dir_from(dir_override: Option<std::ffi::OsString>) -> crate::Result<PathBuf> {
    if let Some(dir) = dir_override {
        return Ok(PathBuf::from(dir));
    }
    use etcetera::AppStrategy as _;
    let args = etcetera::AppStrategyArgs {
        top_level_domain: "dev".to_string(),
        author: "instagent".to_string(),
        app_name: "instagent".to_string(),
    };
    Ok(etcetera::choose_app_strategy(args)
        .context("resolve data dir via etcetera")?
        .data_dir())
}

/// `PLUGIN_DATA`：`<data_dir>/plugins/<name>/`，按需创建（provider 的
/// `${PLUGIN_DATA}` 与插件自带数据共用，第三版 §2.10）。
pub fn plugin_data_dir(name: &str) -> crate::Result<PathBuf> {
    plugin_data_dir_at(&data_dir()?, name)
}

/// [`plugin_data_dir`] 的路径层：数据根目录由参数给出。
pub(crate) fn plugin_data_dir_at(base: &Path, name: &str) -> crate::Result<PathBuf> {
    let dir = base.join("plugins").join(name);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create PLUGIN_DATA dir {}", dir.display()))?;
    Ok(dir)
}

/// 用户插件安装根：`~/.agents/plugins/`（`INSTAGENT_AGENTS_DIR` 可覆盖，`05`）。
fn user_plugins_dir() -> crate::Result<PathBuf> {
    Ok(agents_dir()?.join("plugins"))
}

/// clone / 复制 → 校验 → 写 `.install.json` → 放入用户目录。
/// 同名插件重复 install 即覆盖更新。
pub fn install(source: &InstallSource, opts: &InstallOptions) -> crate::Result<Plugin> {
    let now = now_ts();
    let staging = Staging::new()?;
    let commit = match source {
        InstallSource::GitUrl(url) => {
            if url.trim().is_empty() {
                bail!("plugin git source must not be empty");
            }
            clone_git_repo(url, &staging.path)?;
            let commit = head_commit(&staging.path)?;
            remove_git_dir(&staging.path)?;
            Some(commit)
        }
        InstallSource::Path(path) => {
            std::fs::create_dir_all(&staging.path)?;
            copy_tree(path, &staging.path)?;
            None
        }
    };
    let manifest = read_manifest(&staging.path)
        .with_context(|| format!("validate plugin at {source:?}"))?
        .manifest;
    let info = InstallInfo {
        source: match source {
            InstallSource::GitUrl(url) => url.clone(),
            InstallSource::Path(path) => path.display().to_string(),
        },
        commit,
        installed_at: now,
        // 自动更新的 24h 从安装时刻起算（goose 同款）。
        last_update_check: opts.auto_update.then_some(now),
        auto_update: opts.auto_update,
    };
    let plugin = place(staging, manifest, &info)?;
    Ok(plugin)
}

/// 按 `.install.json` 重新拉取（手动 update 不受节流限制）。
pub fn update(name: &str) -> crate::Result<()> {
    update_at(name, now_ts())
}

/// 24h 节流的纯时间判定（goose `should_auto_update`）。
pub fn should_auto_update(last_update_check: Option<i64>, now: i64) -> bool {
    last_update_check.is_none_or(|checked| now - checked >= AUTO_UPDATE_INTERVAL_SECS)
}

/// 扫描用户插件目录，对 `auto_update` 的 git 来源做节流更新。
/// 失败也返回在结果里（调用方只 warn，goose 同款）。
pub fn auto_update_all(now: i64) -> crate::Result<Vec<AutoUpdateResult>> {
    let root = user_plugins_dir()?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    let mut results = Vec::new();
    for dir in dirs {
        let Ok(info) = read_install_info(&dir) else {
            continue;
        };
        if !info.auto_update || info.commit.is_none() {
            continue;
        }
        if !should_auto_update(info.last_update_check, now) {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name = name.to_string();
        // 先记检查时间再更新：失败也不在每次启动重试。
        let result = mark_last_update_check(&dir, now).and_then(|()| update_at(&name, now));
        results.push(AutoUpdateResult { name, result });
    }
    Ok(results)
}

/// 已安装（用户目录）插件清单；manifest 校验失败的目录跳过
/// （发现层的 skipped 记录在 `05`，这里只报"装了什么"）。
pub fn list(cwd: &Path) -> crate::Result<Vec<InstalledPlugin>> {
    let settings = Settings::merged(cwd)?;
    let root = user_plugins_dir()?;
    let mut installed = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(installed),
        Err(err) => return Err(err.into()),
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir).map(|v| v.manifest) else {
            continue;
        };
        let name = &manifest.name;
        let enabled = if settings.enabled_plugins.is_empty() {
            !settings.disabled_plugins.contains(name)
        } else {
            settings.enabled_plugins.contains(name)
        };
        installed.push(InstalledPlugin {
            plugin: Plugin {
                manifest,
                root: dir.clone(),
                source: PluginSource::User,
            },
            install_info: read_install_info(&dir).ok(),
            enabled,
        });
    }
    installed.sort_by(|a, b| a.plugin.manifest.name.cmp(&b.plugin.manifest.name));
    Ok(installed)
}

/// 单个已安装插件的详情（list 同源）。
pub fn show(cwd: &Path, name: &str) -> crate::Result<InstalledPlugin> {
    list(cwd)?
        .into_iter()
        .find(|item| item.plugin.manifest.name == name)
        .with_context(|| format!("plugin `{name}` is not installed"))
}

/// 启用：写回用户层 settings。黑名单模式下只是移出 `disabledPlugins`；
/// 白名单模式下同时加入 `enabledPlugins`。
pub fn enable(name: &str) -> crate::Result<()> {
    ensure_installed(name)?;
    let mut settings = load_user_settings()?;
    settings.disabled_plugins.retain(|p| p != name);
    if !settings.enabled_plugins.is_empty() && !settings.enabled_plugins.contains(&name.to_string())
    {
        settings.enabled_plugins.push(name.to_string());
    }
    save_user_settings(&settings)
}

/// 禁用：移出 `enabledPlugins` 并同时记入 `disabledPlugins`——后者保证
/// 白名单变空、退化为黑名单模式后依然是禁用态。
pub fn disable(name: &str) -> crate::Result<()> {
    ensure_installed(name)?;
    let mut settings = load_user_settings()?;
    settings.enabled_plugins.retain(|p| p != name);
    if !settings.disabled_plugins.contains(&name.to_string()) {
        settings.disabled_plugins.push(name.to_string());
    }
    save_user_settings(&settings)
}

fn ensure_installed(name: &str) -> crate::Result<()> {
    validate_plugin_name(name)?;
    let dir = user_plugins_dir()?.join(name);
    read_manifest(&dir)
        .with_context(|| format!("plugin `{name}` is not installed at {}", dir.display()))?;
    Ok(())
}

fn load_user_settings() -> crate::Result<Settings> {
    let path = crate::config::config_dir()?.join("settings.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(err.into()),
    }
}

fn save_user_settings(settings: &Settings) -> crate::Result<()> {
    let dir = crate::config::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(settings)?,
    )?;
    Ok(())
}

/// update 的带时刻实现：手动 update 用真实 now，节流测试注入假 now。
fn update_at(name: &str, now: i64) -> crate::Result<()> {
    if name.trim().is_empty() {
        bail!("plugin name must not be empty");
    }
    validate_plugin_name(name)?;
    let dest = user_plugins_dir()?.join(name);
    if !dest.is_dir() {
        bail!("plugin `{name}` is not installed");
    }
    let old = read_install_info(&dest)?;
    if old.commit.is_none() {
        bail!(
            "plugin `{name}` was installed from local path `{}` and cannot be updated",
            old.source
        );
    }
    let staging = Staging::new()?;
    clone_git_repo(&old.source, &staging.path)?;
    let commit = head_commit(&staging.path)?;
    remove_git_dir(&staging.path)?;
    let manifest = read_manifest(&staging.path)
        .with_context(|| format!("validate updated plugin `{name}`"))?
        .manifest;
    if manifest.name != name {
        staging.cleanup_ok();
        bail!(
            "updated plugin name `{}` does not match installed plugin `{name}`",
            manifest.name
        );
    }
    let info = InstallInfo {
        commit: Some(commit),
        last_update_check: Some(now),
        ..old
    };
    place(staging, manifest, &info)?;
    Ok(())
}

/// 把 staging（已含 `.install.json`）换入 `<plugins>/<manifest.name>/`。
fn place(
    mut staging: Staging,
    manifest: PluginManifest,
    info: &InstallInfo,
) -> crate::Result<Plugin> {
    write_install_info(&staging.path, info)?;
    let root = user_plugins_dir()?;
    std::fs::create_dir_all(&root)?;
    let dest = root.join(&manifest.name);
    replace_dir(&staging.path, &dest)?;
    staging.commit();
    Ok(Plugin {
        manifest,
        root: dest,
        source: PluginSource::User,
    })
}

fn read_install_info(dir: &Path) -> crate::Result<InstallInfo> {
    let path = dir.join(INSTALL_METADATA);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "plugin at {} has no {INSTALL_METADATA} and cannot be updated",
            dir.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn write_install_info(dir: &Path, info: &InstallInfo) -> crate::Result<()> {
    let path = dir.join(INSTALL_METADATA);
    std::fs::write(&path, serde_json::to_string_pretty(info)?)?;
    Ok(())
}

fn mark_last_update_check(dir: &Path, now: i64) -> crate::Result<()> {
    let mut info = read_install_info(dir)?;
    info.last_update_check = Some(now);
    write_install_info(dir, &info)
}

/// staging 目录：`<agents_dir>/.tmp-install/<uuid>`——放在用户插件根之外，
/// 不会被 `05` 的发现流程扫到；同盘保证 rename 换入原子。
struct Staging {
    path: PathBuf,
    committed: bool,
}

impl Staging {
    fn new() -> crate::Result<Self> {
        let parent = agents_dir()?.join(".tmp-install");
        std::fs::create_dir_all(&parent)?;
        Ok(Self {
            path: parent.join(Uuid::new_v4().to_string()),
            committed: false,
        })
    }

    fn commit(&mut self) {
        self.committed = true;
    }

    fn cleanup_ok(&self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            self.cleanup_ok();
        }
    }
}

/// 换入 dest：旧目录先挪去同目录备份，任一步失败即回滚（goose 同款）。
fn replace_dir(source: &Path, dest: &Path) -> crate::Result<()> {
    let backup = if dest.exists() {
        let backup = dest.with_file_name(format!(
            ".replaced-{}-{}",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("plugin"),
            Uuid::new_v4()
        ));
        std::fs::rename(dest, &backup).with_context(|| format!("move aside {}", dest.display()))?;
        Some(backup)
    } else {
        None
    };
    match std::fs::rename(source, dest) {
        Ok(()) => {
            if let Some(backup) = backup {
                let _ = std::fs::remove_dir_all(backup);
            }
            Ok(())
        }
        Err(err) => {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, dest);
            }
            Err(err).with_context(|| format!("install into {}", dest.display()))
        }
    }
}

fn clone_git_repo(url: &str, dest: &Path) -> crate::Result<()> {
    let output = git_command()
        .arg("clone")
        .arg("--depth")
        .arg("1")
        // 无网络凭据时直接失败而不是挂起等输入。
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg(url)
        .arg(dest)
        .output()
        .context("failed to run `git clone`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("failed to clone plugin repository: {}", message.trim());
    }
    Ok(())
}

fn head_commit(dir: &Path) -> crate::Result<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .context("failed to run `git rev-parse`")?;
    if !output.status.success() {
        bail!("plugin git source has no commits (rev-parse HEAD failed)");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn remove_git_dir(dir: &Path) -> crate::Result<()> {
    let git = dir.join(".git");
    if git.exists() {
        std::fs::remove_dir_all(&git)?;
    }
    Ok(())
}

/// 递归复制目录树；跳过任意层级的 `.git` 与顶层 `.install.json`
/// （源可能是先前安装过的目录）。
fn copy_tree(source: &Path, dest: &Path) -> crate::Result<()> {
    copy_tree_at(source, dest, true)
}

fn copy_tree_at(source: &Path, dest: &Path, top_level: bool) -> crate::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || (top_level && name == INSTALL_METADATA) {
            continue;
        }
        let target = dest.join(name);
        if entry.file_type()?.is_dir() {
            copy_tree_at(&entry.path(), &target, false)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// env 隔离约定同 `discovery.rs`（lock_env 串行化进程级变量）。
    /// 本模块数据目录逻辑走 `*_at` 参数化入口测试，特意**不**设
    /// `INSTAGENT_DATA_DIR`，保持 hermetic。
    struct Env {
        _guard: MutexGuard<'static, ()>,
        agents: TempDir,
        data: TempDir,
        config: TempDir,
    }

    fn isolated() -> Env {
        let guard = crate::config::lock_env();
        let agents = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let config = TempDir::new().unwrap();
        std::env::set_var("INSTAGENT_AGENTS_DIR", agents.path());
        std::env::set_var("INSTAGENT_CONFIG_DIR", config.path());
        Env {
            _guard: guard,
            agents,
            data,
            config,
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

    fn read_user_settings(env: &Env) -> Settings {
        let text = std::fs::read_to_string(env.config.path().join("settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = git_command().current_dir(cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "`git {args:?}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// 在 `repo` 建一个含 `name` 插件的 git 仓库并 commit，返回 `file://` URL。
    fn init_git_plugin(repo: &Path, name: &str, version: &str) -> String {
        write_plugin(repo, name, version);
        std::fs::create_dir_all(repo.join("dev.instagent/providers")).unwrap();
        std::fs::write(repo.join("dev.instagent/providers/x.json"), "{}").unwrap();
        git(repo, &["-c", "init.defaultBranch=main", "init"]);
        git_commit(repo, &format!("add {name} {version}"));
        format!("file://{}", repo.display())
    }

    fn git_commit(repo: &Path, message: &str) {
        git(
            repo,
            &[
                "-c",
                "user.email=t@instagent.dev",
                "-c",
                "user.name=t",
                "add",
                "-A",
            ],
        );
        git(
            repo,
            &[
                "-c",
                "user.email=t@instagent.dev",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn local_plugin(env: &Env, name: &str, version: &str) -> PathBuf {
        // 模拟"下载好的目录"：放在用户插件目录之外。
        let dir = env.data.path().join(format!("src-{name}"));
        write_plugin(&dir, name, version);
        std::fs::create_dir_all(dir.join("skills/review")).unwrap();
        std::fs::write(dir.join("skills/review/SKILL.md"), "# review").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main").unwrap();
        dir
    }

    #[test]
    fn installs_local_path_without_git_or_metadata_leak() {
        let env = isolated();
        let src = local_plugin(&env, "alpha", "1.0.0");
        let plugin = install(
            &InstallSource::Path(src.clone()),
            &InstallOptions::default(),
        )
        .unwrap();

        assert_eq!(plugin.manifest.name, "alpha");
        assert_eq!(plugin.source, PluginSource::User);
        assert_eq!(plugin.root, env.agents.path().join("plugins").join("alpha"));
        assert!(plugin.root.join("skills/review/SKILL.md").is_file());
        assert!(
            !plugin.root.join(".git").exists(),
            "`.git` must not be copied"
        );

        let info = read_install_info(&plugin.root).unwrap();
        assert_eq!(info.source, src.display().to_string());
        assert_eq!(info.commit, None);
        assert!(!info.auto_update);
        assert_eq!(info.last_update_check, None);
        assert!(info.installed_at > 0);

        // 重复安装覆盖更新（携带新的 manifest 与 metadata）。
        write_plugin(&src, "alpha", "2.0.0");
        let plugin = install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        assert_eq!(plugin.manifest.version, "2.0.0");
        assert_eq!(env.agents.path().join("plugins").join("alpha"), plugin.root);
    }

    #[test]
    fn installs_from_file_git_repo_with_commit() {
        let _env = isolated();
        let repo = TempDir::new().unwrap();
        let url = init_git_plugin(repo.path(), "git-plugin", "1.0.0");
        let plugin = install(
            &InstallSource::GitUrl(url.clone()),
            &InstallOptions { auto_update: true },
        )
        .unwrap();

        assert_eq!(plugin.manifest.name, "git-plugin");
        assert!(!plugin.root.join(".git").exists());
        assert!(plugin.root.join("dev.instagent/providers/x.json").is_file());

        let expected = {
            let out = git_command()
                .current_dir(repo.path())
                .arg("rev-parse")
                .arg("HEAD")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let info = read_install_info(&plugin.root).unwrap();
        assert_eq!(info.source, url);
        assert_eq!(info.commit.as_deref(), Some(expected.as_str()));
        assert!(info.auto_update);
        assert_eq!(info.last_update_check, Some(info.installed_at));
    }

    #[test]
    fn update_refetches_git_source_and_keeps_installed_at() {
        let _env = isolated();
        let repo = TempDir::new().unwrap();
        let url = init_git_plugin(repo.path(), "git-plugin", "1.0.0");
        let plugin = install(&InstallSource::GitUrl(url), &InstallOptions::default()).unwrap();
        let old = read_install_info(&plugin.root).unwrap();

        write_plugin(repo.path(), "git-plugin", "2.0.0");
        git_commit(repo.path(), "bump 2.0.0");
        update("git-plugin").unwrap();

        let updated = crate::plugin::manifest::read_manifest(&plugin.root)
            .unwrap()
            .manifest;
        assert_eq!(updated.version, "2.0.0");
        let info = read_install_info(&plugin.root).unwrap();
        assert_eq!(info.installed_at, old.installed_at);
        assert_ne!(info.commit, old.commit);
        assert!(info.last_update_check.is_some());
    }

    #[test]
    fn update_rejects_local_path_and_unknown_names() {
        let env = isolated();
        let src = local_plugin(&env, "alpha", "1.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        let err = update("alpha").unwrap_err();
        assert!(err.to_string().contains("cannot be updated"), "{err}");

        let err = update("ghost").unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[test]
    fn auto_update_throttles_at_24h() {
        let env = isolated();
        let repo = TempDir::new().unwrap();
        let url = init_git_plugin(repo.path(), "git-plugin", "1.0.0");
        let plugin = install(
            &InstallSource::GitUrl(url),
            &InstallOptions { auto_update: true },
        )
        .unwrap();
        let installed_at = read_install_info(&plugin.root).unwrap().installed_at;
        assert!(
            installed_at > 0 && installed_at <= now_ts(),
            "just installed"
        );

        // 24h 内：节流，一条都不跑。
        let results = auto_update_all(installed_at + 60 * 60).unwrap();
        assert!(results.is_empty(), "{results:?}");

        // 过期后：更新成功，检查时间前移。
        write_plugin(repo.path(), "git-plugin", "2.0.0");
        git_commit(repo.path(), "bump 2.0.0");
        let late = installed_at + AUTO_UPDATE_INTERVAL_SECS;
        let results = auto_update_all(late).unwrap();
        assert_eq!(results.len(), 1);
        results[0].result.as_ref().expect("auto update ok");
        assert_eq!(results[0].name, "git-plugin");
        let info = read_install_info(&plugin.root).unwrap();
        assert_eq!(info.last_update_check, Some(late));
        assert_eq!(
            crate::plugin::manifest::read_manifest(&plugin.root)
                .unwrap()
                .manifest
                .version,
            "2.0.0"
        );

        // 紧接着再跑：又被节流。
        let results = auto_update_all(late + 60).unwrap();
        assert!(results.is_empty());

        // 非 git 来源与未开 auto_update 的插件被跳过。
        let src = local_plugin(&env, "alpha", "1.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        let results = auto_update_all(late + 100 * AUTO_UPDATE_INTERVAL_SECS).unwrap();
        assert!(results.iter().all(|r| r.name != "alpha"));
    }

    #[test]
    fn should_auto_update_time_rule() {
        assert!(should_auto_update(None, 100));
        assert!(!should_auto_update(
            Some(100),
            100 + AUTO_UPDATE_INTERVAL_SECS - 1
        ));
        assert!(should_auto_update(
            Some(100),
            100 + AUTO_UPDATE_INTERVAL_SECS
        ));
    }

    #[test]
    fn list_show_and_enable_disable_round_trip() {
        let env = isolated();
        let cwd = env.agents.path();
        for name in ["a", "b"] {
            let src = local_plugin(&env, name, "1.0.0");
            install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        }

        let items = list(cwd).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|i| i.plugin.manifest.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(items.iter().all(|i| i.enabled));
        assert!(items.iter().all(|i| i.install_info.is_some()));

        disable("b").unwrap();
        assert_eq!(
            read_user_settings(&env).disabled_plugins,
            vec!["b".to_string()]
        );
        let items = list(cwd).unwrap();
        assert_eq!(
            items
                .iter()
                .filter(|i| i.enabled)
                .map(|i| i.plugin.manifest.name.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        let shown = show(cwd, "a").unwrap();
        assert!(shown.enabled);
        assert!(show(cwd, "ghost").is_err());

        // 黑名单模式下 enable 只需移出 disabled。
        enable("b").unwrap();
        assert!(list(cwd).unwrap().iter().all(|i| i.enabled));
        assert!(read_user_settings(&env).disabled_plugins.is_empty());
        assert!(read_user_settings(&env).enabled_plugins.is_empty());

        // 进入白名单模式（a、b 显式启用）后：enable 未安装的直接报错；
        // disable 从 enabled 移除并记入 disabled，防止白名单清空后模式翻转复活。
        let settings = Settings {
            enabled_plugins: vec!["a".into(), "b".into()],
            ..Settings::default()
        };
        settings
            .save(cwd, crate::settings::SettingsLayer::User)
            .unwrap();
        assert!(enable("c").is_err()); // c 未安装。
        assert!(disable("c").is_err());
        disable("b").unwrap();
        let after = read_user_settings(&env);
        assert_eq!(after.enabled_plugins, vec!["a".to_string()]);
        assert_eq!(after.disabled_plugins, vec!["b".to_string()]);
        assert_eq!(
            list(cwd)
                .unwrap()
                .iter()
                .filter(|i| i.enabled)
                .map(|i| i.plugin.manifest.name.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );

        disable("a").unwrap();
        let after = read_user_settings(&env);
        assert!(after.enabled_plugins.is_empty());
        assert_eq!(
            after.disabled_plugins,
            vec!["b".to_string(), "a".to_string()]
        );
        assert!(!list(cwd).unwrap().iter().any(|i| i.enabled));
    }

    #[test]
    fn plugin_data_dir_lives_under_data_dir_and_creates_on_demand() {
        let base = TempDir::new().unwrap();
        let dir = plugin_data_dir_at(base.path(), "alpha").unwrap();
        assert_eq!(dir, base.path().join("plugins").join("alpha"));
        assert!(dir.is_dir());
    }

    #[test]
    fn data_dir_honors_override_without_touching_process_env() {
        let base = TempDir::new().unwrap();
        let resolved = data_dir_from(Some(base.path().into())).unwrap();
        assert_eq!(resolved, base.path());
    }

    #[test]
    fn install_rejects_invalid_sources() {
        let _env = isolated();
        let err = install(
            &InstallSource::GitUrl("  ".into()),
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");

        let missing = TempDir::new().unwrap();
        let err = install(
            &InstallSource::Path(missing.path().join("nope")),
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        // staging 用完即清，不残留。
        let tmp_root = agents_dir().unwrap().join(".tmp-install");
        assert!(
            !tmp_root.exists() || std::fs::read_dir(&tmp_root).unwrap().next().is_none(),
            "staging left behind in {tmp_root:?}"
        );
    }
}
