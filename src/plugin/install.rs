//! 插件安装 / 更新（第三版 §2.10；逻辑参考 goose `plugins/mod.rs`，只读）。
//!
//! `install`：git-url 经统一 Tokio subprocess wrapper clone（进程组 +
//! `kill_on_drop` + 超时 + bounded output，超限/超时/取消整组 SIGKILL；
//! 不引 libgit2），本地路径直接复制 → `04` 校验 `plugin.json` → 写 `.install.json`
//! （source、commit、时间）→ 放入 `~/.agents/plugins/<name>/`。staging +
//! rename 替换，旧目录在替换成功前不动。
//! `update`：按 `.install.json` 重新拉取；`commit` 为 `None` 即本地路径
//! 安装，无法 update（goose 同款 `source_type != "git"` 拒绝，InstallInfo
//! 字段由 `00` 锁定，不另存 source_type）。自动更新 24h 节流：
//! [`auto_update_all`] 只处理 `auto_update` 且距 [`should_auto_update`]
//! 到期的插件，失败也先记检查时间，避免每次启动重试（goose 同款）。
//! list / show / enable / disable 为数据层，CLI 接线在 `18`。
//! `PLUGIN_DATA`：[`plugin_data_dir`] = `<data_dir>/plugins/<name>/`，按需创建。
//!
//! 实现按职责分为五个私有子模块（公开 API 全部留在本模块）：
//! `acquire` source acquisition（git clone / rev-parse / 错误回显与 URL
//! 脱敏）、`metadata` manifest/metadata persistence（`.install.json`
//! 读写）、`staging` staging 目录与本地复制（symlink 契约）、
//! `replace` 原子替换与 `.replaced-*` 可恢复状态机、
//! `update` update / auto-update 节流。

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

use crate::plugin::discovery::agents_dir;
use crate::plugin::manifest::read_manifest;
use crate::plugin::manifest::validate_plugin_name;
use crate::plugin::manifest::PluginManifest;
use crate::plugin::plugin_enabled;
use crate::plugin::Plugin;
use crate::plugin::PluginSource;
use crate::settings::Settings;

use acquire::fetch_git_commit;
use acquire::redact_url;
use acquire::remove_git_dir;
use metadata::read_install_info;
use replace::place;
use staging::copy_tree;
use staging::Staging;

pub use update::auto_update_all;
pub use update::should_auto_update;
pub use update::update;

/// `.install.json` 文件名（goose `.goose-plugin-install.json` 的对应物）。
pub const INSTALL_METADATA: &str = ".install.json";

/// 24h 自动更新节流间隔（goose `AUTO_UPDATE_INTERVAL_HOURS`）。
pub const AUTO_UPDATE_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// `git clone` 整体超时：到点整组 SIGKILL，不留挂起进程。
pub const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(120);

/// `git rev-parse` 这类本地快命令的超时。
pub const GIT_QUICK_TIMEOUT: Duration = Duration::from_secs(15);

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

/// 安装根下的内部状态目录，不是插件：`.replaced-*` 替换备份（`replace`）
/// 与 `.tmp-install` staging（`staging`）。所有安装根扫描（list /
/// auto-update / discovery）一律跳过，不把正在安装或待恢复的副本当成插件
/// 发现、计数或更新（I02）。只按目录身份排除，绝不据此删除任何数据。
pub(crate) fn is_install_internal_dir(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.starts_with(replace::REPLACED_PREFIX) || name == staging::STAGING_DIR_NAME
}

/// clone / 复制 → 校验 → 写 `.install.json` → 放入用户目录。
/// 同名插件重复 install 即覆盖更新。
pub fn install(source: &InstallSource, opts: &InstallOptions) -> crate::Result<Plugin> {
    let now = crate::message::now_ts();
    let staging = Staging::new()?;
    let commit = match source {
        InstallSource::GitUrl(url) => {
            if url.trim().is_empty() {
                bail!("plugin git source must not be empty");
            }
            let commit = fetch_git_commit(url, &staging.path)?;
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
        .with_context(|| format!("validate plugin at {}", source_display(source)))?;
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
        if !dir.is_dir() || is_install_internal_dir(&entry.file_name()) {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        let name = &manifest.name;
        let enabled = plugin_enabled(name, &settings);
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
/// 白名单模式（含显式 `[]` 终值）下同时加入 `enabledPlugins`——判模式走
/// [`Settings::whitelist`]，不能用 `enabled_plugins.is_empty()`，否则
/// `enabledPlugins: []` 后 `plugin enable` 永远写不进白名单（I01）。
pub fn enable(name: &str) -> crate::Result<()> {
    ensure_installed(name)?;
    let mut settings = Settings::load_user()?;
    settings.disabled_plugins.retain(|p| p != name);
    if settings.whitelist().is_some() && !settings.enabled_plugins.contains(&name.to_string()) {
        settings.enabled_plugins.push(name.to_string());
        // 白名单转由非空集合表达，空集合标志清掉保持互斥。
        settings.enabled_locked = false;
    }
    settings.save_user()
}

/// 禁用：移出 `enabledPlugins` 并同时记入 `disabledPlugins`。移除最后一项
/// 后白名单模式保留（I01）：空集合按显式 `[]` 写回，无关插件不会因模式
/// 翻转成黑名单而复活；`disabledPlugins` 里的名字进一步防漂移。
pub fn disable(name: &str) -> crate::Result<()> {
    ensure_installed(name)?;
    let mut settings = Settings::load_user()?;
    let whitelist_mode = settings.whitelist().is_some();
    settings.enabled_plugins.retain(|p| p != name);
    if whitelist_mode && settings.enabled_plugins.is_empty() {
        settings.enabled_locked = true;
    }
    if !settings.disabled_plugins.contains(&name.to_string()) {
        settings.disabled_plugins.push(name.to_string());
    }
    settings.save_user()
}

fn ensure_installed(name: &str) -> crate::Result<()> {
    validate_plugin_name(name)?;
    let dir = user_plugins_dir()?.join(name);
    read_manifest(&dir)
        .with_context(|| format!("plugin `{name}` is not installed at {}", dir.display()))?;
    Ok(())
}

/// source acquisition：git-url 的 clone / rev-parse（统一 Tokio wrapper、
/// 进程组 + 超时 + bounded output + 取消）与失败回显脱敏。
mod acquire {
    use std::future::Future;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::bail;
    use anyhow::Context;
    use tokio_util::sync::CancellationToken;

    use crate::subprocess::git_command;
    use crate::subprocess::run_bounded;
    use crate::subprocess::CollectedRun;
    use crate::subprocess::Outcome;
    use crate::subprocess::ProcessGroupChild;

    use super::GIT_CLONE_TIMEOUT;
    use super::GIT_QUICK_TIMEOUT;

    /// git 子进程单路输出的收集硬上限（超出即杀组并只保留头部摘要）。
    pub(super) const GIT_OUTPUT_CAP_BYTES: usize = 64 * 1024;

    /// 错误信息里回显 git 输出的字节上限（只显示截断摘要，绝不整段倾倒）。
    const GIT_ERROR_DISPLAY_BYTES: usize = 512;

    /// clone → rev-parse HEAD：一次 git 拉取的可取消整体（同步入口用的胶水）。
    pub(super) fn fetch_git_commit(url: &str, dest: &Path) -> crate::Result<String> {
        let url = url.to_string();
        let dest = dest.to_path_buf();
        block_on_dedicated(async move {
            clone_git_repo(&url, &dest, GIT_CLONE_TIMEOUT, None).await?;
            head_commit(&dest, GIT_QUICK_TIMEOUT, None).await
        })
    }

    /// 公共 install/update 是同步入口（CLI handlers 与 assembly 未 async 化），
    /// 而 git 流程要跑在 Tokio wrapper 上。专用 OS 线程 + 私有 current-thread
    /// runtime：既能在"已在 runtime 里"的调用线程上安全执行（同线程嵌套
    /// block_on 会 panic），也把挂起/超时/杀组的取消语义完整保留。
    fn block_on_dedicated<T: Send + 'static>(
        future: impl Future<Output = crate::Result<T>> + Send + 'static,
    ) -> crate::Result<T> {
        std::thread::Builder::new()
            .name("instagent-git".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("create git subprocess runtime")?;
                runtime.block_on(future)
            })
            .context("spawn git subprocess thread")?
            .join()
            .map_err(|_| anyhow::anyhow!("git subprocess thread panicked"))?
    }

    /// 统一 git 子进程入口：`git_command` 的加固参数 + 进程组 / `kill_on_drop`
    /// （[`ProcessGroupChild`]）、超时、取消与双路 bounded output（[`run_bounded`]）。
    /// 超时 / 取消 / 输出越限时整组（含孙进程）SIGKILL，不残留。
    async fn run_git(
        command: &mut tokio::process::Command,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> crate::Result<CollectedRun> {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(command).context("failed to spawn `git`")?;
        run_bounded(child, GIT_OUTPUT_CAP_BYTES, timeout, cancel)
            .await
            .context("collect git subprocess output")
    }

    fn git_command_async() -> tokio::process::Command {
        tokio::process::Command::from(git_command())
    }

    pub(super) async fn clone_git_repo(
        url: &str,
        dest: &Path,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> crate::Result<()> {
        let mut command = git_command_async();
        command
            .arg("clone")
            .arg("--depth")
            .arg("1")
            // 无网络凭据时直接失败而不是挂起等输入。
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg(url)
            .arg(dest);
        let run = run_git(&mut command, timeout, cancel).await?;
        if run.outcome == Outcome::Exited(Some(0)) {
            return Ok(());
        }
        let message = match run.outcome {
            Outcome::TimedOut => format!("timed out cloning plugin repository after {timeout:?}"),
            Outcome::Cancelled => "cancelled while cloning plugin repository".to_string(),
            _ => format!(
                "failed to clone plugin repository: {}",
                git_error_summary(&run, Some(url))
            ),
        };
        bail!(message)
    }

    async fn head_commit(
        dir: &Path,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> crate::Result<String> {
        let mut command = git_command_async();
        command.arg("-C").arg(dir).arg("rev-parse").arg("HEAD");
        let run = run_git(&mut command, timeout, cancel).await?;
        let commit = run.stdout.text.trim().to_string();
        if run.outcome != Outcome::Exited(Some(0)) || commit.is_empty() {
            bail!(
                "plugin git source has no commits (rev-parse HEAD failed): {}",
                git_error_summary(&run, None)
            );
        }
        Ok(commit)
    }

    /// git 失败的可读摘要：stderr 优先（git 的诊断走 stderr），去凭据、封顶截断。
    fn git_error_summary(run: &CollectedRun, redact: Option<&str>) -> String {
        let stream = if run.stderr.text.trim().is_empty() {
            &run.stdout
        } else {
            &run.stderr
        };
        let mut text = stream.text.trim().to_string();
        if let Some(url) = redact {
            text = text.replace(url, "<git-url>");
        }
        if text.len() > GIT_ERROR_DISPLAY_BYTES {
            let mut cut = GIT_ERROR_DISPLAY_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push('…');
        }
        if let Some(note) = stream.truncation_note() {
            text.push('\n');
            text.push_str(&note);
        }
        if text.is_empty() {
            text = "(no git output)".to_string();
        }
        text
    }

    /// 去掉 URL 里的 `user:password@` 凭据段，用于错误展示（不落日志、不回显）。
    pub(super) fn redact_url(url: &str) -> String {
        let Some(scheme_end) = url.find("://") else {
            return url.to_string();
        };
        let authority_start = scheme_end + 3;
        let authority_end = url[authority_start..]
            .find(['/', '?', '#'])
            .map_or(url.len(), |offset| authority_start + offset);
        match url[authority_start..authority_end].rfind('@') {
            Some(at) => format!(
                "{}{}",
                &url[..authority_start],
                &url[authority_start + at..]
            ),
            None => url.to_string(),
        }
    }

    pub(super) fn remove_git_dir(dir: &Path) -> crate::Result<()> {
        let git = dir.join(".git");
        if git.exists() {
            std::fs::remove_dir_all(&git)?;
        }
        Ok(())
    }
}

/// manifest/metadata persistence：`.install.json` 的读写（原子私有写入）。
mod metadata {
    use std::path::Path;

    use anyhow::Context;

    use super::InstallInfo;
    use super::INSTALL_METADATA;

    pub(super) fn read_install_info(dir: &Path) -> crate::Result<InstallInfo> {
        let path = dir.join(INSTALL_METADATA);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "plugin at {} has no {INSTALL_METADATA} and cannot be updated",
                dir.display()
            )
        })?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub(super) fn write_install_info(dir: &Path, info: &InstallInfo) -> crate::Result<()> {
        let path = dir.join(INSTALL_METADATA);
        crate::settings::write_private_atomic(&path, &serde_json::to_string_pretty(info)?)
    }

    pub(super) fn mark_last_update_check(dir: &Path, now: i64) -> crate::Result<()> {
        let mut info = read_install_info(dir)?;
        info.last_update_check = Some(now);
        write_install_info(dir, &info)
    }
}

/// staging/copy：staging 目录（同盘 rename 前提）与本地复制树的 symlink 契约。
mod staging {
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::SystemTime;

    use anyhow::bail;
    use anyhow::Context;
    use uuid::Uuid;

    use super::agents_dir;
    use super::INSTALL_METADATA;

    /// 崩溃进程遗留在 `.tmp-install` 下的 staging 孤儿超过这个时长即清理。
    pub(super) const STAGING_MAX_AGE: Duration = Duration::from_secs(60 * 60);

    /// staging 父目录名（安装根扫描的排除项之一，见 [`super::is_install_internal_dir`]）。
    pub(super) const STAGING_DIR_NAME: &str = ".tmp-install";

    /// staging 目录：`<agents_dir>/.tmp-install/<uuid>`——放在用户插件根之外，
    /// 不会被 `05` 的发现流程扫到；同盘保证 rename 换入原子。
    pub(super) struct Staging {
        pub(super) path: PathBuf,
        committed: bool,
    }

    impl Staging {
        pub(super) fn new() -> crate::Result<Self> {
            let parent = agents_dir()?.join(STAGING_DIR_NAME);
            std::fs::create_dir_all(&parent)?;
            cleanup_stale_staging(&parent, SystemTime::now());
            Ok(Self {
                path: parent.join(Uuid::new_v4().to_string()),
                committed: false,
            })
        }

        pub(super) fn commit(&mut self) {
            self.committed = true;
        }

        pub(super) fn cleanup_ok(&self) {
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

    /// 清理策略：`.tmp-install` 下 mtime 老于 [`STAGING_MAX_AGE`] 的条目一定是
    /// 崩溃进程遗留（活着的 staging 在当次进程内由 Drop 删除），下次建 staging 前扫掉。
    /// `now` 参数化以便测试注入假时钟。
    pub(super) fn cleanup_stale_staging(parent: &Path, now: SystemTime) {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok();
            let stale = modified
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > STAGING_MAX_AGE);
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// 递归复制目录树；跳过任意层级的 `.git` 与顶层 `.install.json`
    /// （源可能是先前安装过的目录）。
    ///
    /// symlink 契约：源树中**任何**层级的 symlink（指向文件或目录皆同）一律
    /// 显式拒绝并报出路径，既不复制链接本身也不跟随其目标——防止 staging
    /// 借道指向源树外的文件（如 `~/.ssh`）；非目录非文件的特殊文件（fifo、
    /// socket、设备）同样拒绝。
    pub(super) fn copy_tree(source: &Path, dest: &Path) -> crate::Result<()> {
        copy_tree_at(source, dest, true)
    }

    fn copy_tree_at(source: &Path, dest: &Path, top_level: bool) -> crate::Result<()> {
        std::fs::create_dir_all(dest)?;
        for entry in
            std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if name == ".git" || (top_level && name == INSTALL_METADATA) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                bail!(
                    "plugin source contains symlink `{}`; symlinks are rejected, not copied",
                    entry.path().display()
                );
            }
            let target = dest.join(name);
            if file_type.is_dir() {
                copy_tree_at(&entry.path(), &target, false)?;
            } else if file_type.is_file() {
                std::fs::copy(entry.path(), &target)?;
            } else {
                bail!(
                    "plugin source contains non-regular file `{}`",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }
}

/// atomic replacement：`.replaced-*` 备份 + 换入失败回滚的可恢复状态机。
mod replace {
    use std::path::Path;

    use anyhow::Context;
    use uuid::Uuid;

    use super::metadata::write_install_info;
    use super::staging::Staging;
    use super::user_plugins_dir;
    use super::InstallInfo;
    use super::Plugin;
    use super::PluginManifest;
    use super::PluginSource;

    /// `replace_dir` 换入前挪走旧目录的备份前缀。
    pub(super) const REPLACED_PREFIX: &str = ".replaced-";

    /// 把 staging（已含 `.install.json`）换入 `<plugins>/<manifest.name>/`。
    pub(super) fn place(
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

    /// 换入 dest：旧目录先挪去同目录备份，换入失败即回滚（goose 同款）。
    /// 只有本次换入成功后才删除本次创建的那一份备份；其它 `.replaced-*`
    /// 备份（未知归属、并发安装、回滚失败的产物）一律保留——它们可能是
    /// 某个旧版本仅剩的可恢复副本（I02），不按年龄或前缀做全局清理。
    /// 回滚也失败时绝不静默吞掉：错误指明可手动恢复的备份路径。
    pub(super) fn replace_dir(source: &Path, dest: &Path) -> crate::Result<()> {
        let backup = if dest.exists() {
            let backup = dest.with_file_name(format!(
                "{REPLACED_PREFIX}{}-{}",
                dest.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("plugin"),
                Uuid::new_v4()
            ));
            std::fs::rename(dest, &backup)
                .with_context(|| format!("move aside {}", dest.display()))?;
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
                    if let Err(restore_err) = std::fs::rename(backup, dest) {
                        return Err(anyhow::Error::from(err).context(format!(
                            "install into {} failed and the old plugin could not be moved back; \
                             recover it manually from {} ({restore_err})",
                            dest.display(),
                            backup.display()
                        )));
                    }
                }
                Err(err).with_context(|| format!("install into {}", dest.display()))
            }
        }
    }
}

/// update / auto-update：手动 update 与 24h 节流的批量更新。
mod update {
    use anyhow::bail;
    use anyhow::Context;

    use super::acquire::fetch_git_commit;
    use super::acquire::remove_git_dir;
    use super::is_install_internal_dir;
    use super::metadata::mark_last_update_check;
    use super::metadata::read_install_info;
    use super::replace::place;
    use super::staging::Staging;
    use super::user_plugins_dir;
    use super::validate_plugin_name;
    use super::AutoUpdateResult;
    use super::InstallInfo;
    use super::AUTO_UPDATE_INTERVAL_SECS;

    use crate::plugin::manifest::read_manifest;

    /// 按 `.install.json` 重新拉取（手动 update 不受节流限制）。
    pub fn update(name: &str) -> crate::Result<()> {
        update_at(name, crate::message::now_ts())
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
        let mut dirs: Vec<std::path::PathBuf> = entries
            .flatten()
            .filter(|entry| !is_install_internal_dir(&entry.file_name()))
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

    /// update 的带时刻实现：手动 update 用真实 now，节流测试注入假 now。
    pub(super) fn update_at(name: &str, now: i64) -> crate::Result<()> {
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
        let commit = fetch_git_commit(&old.source, &staging.path)?;
        remove_git_dir(&staging.path)?;
        let manifest = read_manifest(&staging.path)
            .with_context(|| format!("validate updated plugin `{name}`"))?;
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
}

fn source_display(source: &InstallSource) -> String {
    match source {
        InstallSource::GitUrl(url) => redact_url(url),
        InstallSource::Path(path) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use std::sync::MutexGuard;
    use std::time::SystemTime;
    use tempfile::TempDir;

    use super::acquire::clone_git_repo;
    use super::acquire::redact_url;
    use super::acquire::GIT_OUTPUT_CAP_BYTES;
    use super::metadata::read_install_info;
    use super::metadata::write_install_info;
    use super::replace::replace_dir;
    use super::replace::REPLACED_PREFIX;
    use super::staging::cleanup_stale_staging;
    use super::staging::STAGING_MAX_AGE;

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
        let output = crate::subprocess::git_command()
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
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
            let out = crate::subprocess::git_command()
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

        let updated = crate::plugin::manifest::read_manifest(&plugin.root).unwrap();
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
            installed_at > 0 && installed_at <= crate::message::now_ts(),
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
    fn list_reports_disabled_for_explicit_empty_whitelist() {
        // 15 的残留闭环：显式 `enabledPlugins: []` = 禁用全部（ADR 0003 D5），
        // list 的 enabled 列不得读成"全部已启用"。
        let env = isolated();
        let cwd = env.agents.path();
        let src = local_plugin(&env, "alpha", "1.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();

        std::fs::write(
            env.config.path().join("settings.json"),
            r#"{"enabledPlugins":[]}"#,
        )
        .unwrap();
        let items = list(cwd).unwrap();
        assert_eq!(items.len(), 1);
        assert!(!items[0].enabled);
        assert!(!show(cwd, "alpha").unwrap().enabled);
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

    #[cfg(unix)]
    #[test]
    fn install_and_enable_writes_are_private_and_atomic() {
        use std::os::unix::fs::PermissionsExt;
        let env = isolated();
        let src = local_plugin(&env, "alpha", "1.0.0");
        let plugin = install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        disable("alpha").unwrap();

        for path in [
            plugin.root.join(INSTALL_METADATA),
            env.config.path().join("settings.json"),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "private mode for {}", path.display());
        }
        // 同目录不留原子写入的临时文件。
        for dir in [plugin.root.clone(), env.config.path().to_path_buf()] {
            assert!(
                std::fs::read_dir(&dir)
                    .unwrap()
                    .flatten()
                    .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
                "temp file left in {}",
                dir.display()
            );
        }
        assert_eq!(read_user_settings(&env).disabled_plugins, vec!["alpha"]);
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

    #[test]
    fn git_clone_failure_leaves_no_staging_or_partial_target() {
        let env = isolated();
        let err = install(
            &InstallSource::GitUrl("file:///definitely-not-a-repo-42".into()),
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to clone"), "{err}");
        let tmp_root = agents_dir().unwrap().join(".tmp-install");
        assert!(
            !tmp_root.exists() || std::fs::read_dir(&tmp_root).unwrap().next().is_none(),
            "staging left behind in {tmp_root:?}"
        );
        let root = env.agents.path().join("plugins");
        assert!(
            !root.exists() || std::fs::read_dir(&root).unwrap().next().is_none(),
            "half install under {root:?}"
        );
    }

    #[test]
    fn replace_dir_rolls_old_plugin_back_when_swap_fails() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("alpha");
        write_plugin(&dest, "alpha", "1.0.0");
        let missing_source = dir.path().join("no-such-staging");

        let err = replace_dir(&missing_source, &dest).unwrap_err();
        assert!(err.to_string().contains("install into"), "{err}");
        assert!(
            dest.join("plugin.json").is_file(),
            "old plugin must be back in place after a failed swap"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(REPLACED_PREFIX)),
            "rollback must not leave a .replaced-* backup behind"
        );
    }

    /// I02：别人的 / 未知归属的 `.replaced-*` 备份不是本次替换的产物，
    /// 安装其它插件时必须原样保留（可能是某旧版本仅剩的可恢复副本）。
    #[test]
    fn replaced_backups_survive_unrelated_installs() {
        let env = isolated();
        let root = env.agents.path().join("plugins");
        // lost 仅剩的唯一恢复备份。
        let lost = root.join(format!("{REPLACED_PREFIX}lost-6f9b2c4e"));
        std::fs::create_dir_all(&lost).unwrap();
        std::fs::write(lost.join("recover-me"), b"sole copy").unwrap();
        // 模拟另一个活动替换：beta 已挪走备份，新版本尚未换入。
        let active = root.join(format!("{REPLACED_PREFIX}beta-1a2b3c4d"));
        write_plugin(&active, "beta", "1.0.0");

        let src = local_plugin(&env, "newplugin", "1.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();

        assert_eq!(
            std::fs::read(lost.join("recover-me")).unwrap(),
            b"sole copy",
            "lost 的唯一恢复备份内容不得改变"
        );
        assert!(
            active.join("plugin.json").is_file(),
            "另一个活动替换的备份不得被删除"
        );
    }

    /// I02：成功替换只清理本次拥有的备份；别的备份与失败回滚的备份保留。
    #[test]
    fn successful_replace_removes_only_its_own_backup() {
        let env = isolated();
        let src = local_plugin(&env, "alpha", "1.0.0");
        install(
            &InstallSource::Path(src.clone()),
            &InstallOptions::default(),
        )
        .unwrap();
        let root = env.agents.path().join("plugins");
        let foreign = root.join(format!("{REPLACED_PREFIX}other-deadbeef"));
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("keep"), b"keep").unwrap();

        write_plugin(&src, "alpha", "2.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();

        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(REPLACED_PREFIX))
            .collect();
        assert_eq!(
            leftovers,
            [format!("{REPLACED_PREFIX}other-deadbeef")],
            "本次 replace 只应清掉自己的备份"
        );
        assert!(foreign.join("keep").is_file());
    }

    /// I02：`.replaced-*` 与 `.tmp-install` 不出现在 list / auto-update 里，
    /// 待恢复副本不会被当成插件或更新对象。
    #[test]
    fn internal_dirs_stay_out_of_list_and_auto_update() {
        let env = isolated();
        let src = local_plugin(&env, "alpha", "2.0.0");
        install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        let root = env.agents.path().join("plugins");
        // 同名旧版本备份：带 auto-update 元数据，证明 auto_update 也不碰它。
        let backup = root.join(format!("{REPLACED_PREFIX}alpha-cafebabe"));
        write_plugin(&backup, "alpha", "1.0.0");
        write_install_info(
            &backup,
            &InstallInfo {
                source: "file:///nonexistent/repo".into(),
                commit: Some("0000000".into()),
                installed_at: 1,
                last_update_check: None,
                auto_update: true,
            },
        )
        .unwrap();
        // staging 内部目录。
        let staging = root.join(".tmp-install").join("half");
        write_plugin(&staging, "beta", "1.0.0");

        let items = list(env.agents.path()).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.plugin.manifest.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha"],
            "备份 / staging 不得出现在 list"
        );
        assert_eq!(items[0].plugin.manifest.version, "2.0.0", "必须是活插件");
        assert_eq!(items[0].plugin.root, root.join("alpha"));

        // 备份若被扫到，会因 file:// 源不可达产生失败结果；空结果即排除生效。
        let results =
            auto_update_all(crate::message::now_ts() + AUTO_UPDATE_INTERVAL_SECS).unwrap();
        assert!(results.is_empty(), "{results:?}");
    }

    /// I01：显式 `enabledPlugins: []` 后 enable 写入白名单，且只启用它。
    #[test]
    fn enable_under_explicit_empty_whitelist_enables_only_that_plugin() {
        let env = isolated();
        let cwd = env.agents.path();
        for name in ["alpha", "beta"] {
            let src = local_plugin(&env, name, "1.0.0");
            install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        }
        std::fs::write(
            env.config.path().join("settings.json"),
            r#"{"enabledPlugins":[]}"#,
        )
        .unwrap();

        enable("alpha").unwrap();
        let after = read_user_settings(&env);
        assert_eq!(after.enabled_plugins, vec!["alpha".to_string()]);

        let items = list(cwd).unwrap();
        let enabled: Vec<&str> = items
            .iter()
            .filter(|item| item.enabled)
            .map(|item| item.plugin.manifest.name.as_str())
            .collect();
        assert_eq!(enabled, ["alpha"], "显式 [] 后 enable 仅启用该插件");
    }

    /// I01：白名单禁用最后一项后仍是白名单模式（显式 `[]` 写回），
    /// 未列入的插件不因模式翻转而复活。
    #[test]
    fn disable_last_whitelisted_plugin_keeps_whitelist_mode() {
        let env = isolated();
        let cwd = env.agents.path();
        for name in ["alpha", "beta"] {
            let src = local_plugin(&env, name, "1.0.0");
            install(&InstallSource::Path(src), &InstallOptions::default()).unwrap();
        }
        std::fs::write(
            env.config.path().join("settings.json"),
            r#"{"enabledPlugins":["alpha"]}"#,
        )
        .unwrap();

        disable("alpha").unwrap();
        let after = read_user_settings(&env);
        assert_eq!(after.whitelist(), Some(&[][..]), "[] 必须显式写回");

        let items = list(cwd).unwrap();
        assert!(
            items.iter().all(|item| !item.enabled),
            "beta 不得因白名单清空而启用"
        );
    }

    #[test]
    fn stale_staging_swept_by_injected_clock() {
        let parent = TempDir::new().unwrap();
        let entry = parent.path().join("crashed-install");
        std::fs::create_dir_all(&entry).unwrap();

        // 新鲜（now 与 mtime 同刻）：不动。
        cleanup_stale_staging(parent.path(), SystemTime::now());
        assert!(entry.exists());

        // 假时钟前进超过 TTL：崩溃遗留被扫掉。
        cleanup_stale_staging(parent.path(), SystemTime::now() + STAGING_MAX_AGE * 2);
        assert!(!entry.exists());
    }

    #[test]
    fn redact_url_strips_credentials_only() {
        assert_eq!(
            redact_url("https://user:sekret@gitlab.example/x/y.git"),
            "https://@gitlab.example/x/y.git"
        );
        assert_eq!(redact_url("file:///tmp/repo"), "file:///tmp/repo");
        assert_eq!(redact_url("https://host/x"), "https://host/x");
        assert_eq!(
            redact_url("git@github.com:owner/repo.git"),
            "git@github.com:owner/repo.git"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_rejects_symlinks_at_any_level() {
        let env = isolated();
        let src = env.data.path().join("linksrc");
        write_plugin(&src, "alpha", "1.0.0");
        std::fs::create_dir_all(src.join("skills")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.join("escape")).unwrap();
        std::os::unix::fs::symlink("/tmp", src.join("skills/dirlink")).unwrap();

        let err = install(&InstallSource::Path(src), &InstallOptions::default()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("symlink"), "{message}");
        assert!(
            !env.agents.path().join("plugins").join("alpha").exists(),
            "rejected install must not create the target"
        );
    }

    // ---- fake git：超时 / 取消 / 输出洪泛 / 失败回显 的进程组回收 ----

    #[cfg(unix)]
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    #[cfg(unix)]
    fn signalable(pid: i32) -> bool {
        unsafe { kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    fn read_pid(path: &Path) -> i32 {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    return pid;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no pid recorded in {}", path.display());
    }

    #[cfg(unix)]
    async fn wait_group_gone(child_pid: i32, grandchild_pid: Option<i32>) {
        for _ in 0..200 {
            let child_gone = !signalable(child_pid) && !signalable(-child_pid);
            let grandchild_gone = grandchild_pid.is_none_or(|pid| !signalable(pid));
            if child_gone && grandchild_gone {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "fake git process group still alive: child {child_pid}, \
             grandchild {grandchild_pid:?}, group {}",
            -child_pid
        );
    }

    /// 把 fake git（POSIX shell 脚本）插到 PATH 最前；Drop 恢复 PATH。
    /// lock_env 与其它改环境变量的测试互斥（fake git 在 PATH 上时，
    /// 别的测试起的真 git 会中招，必须串行）。
    #[cfg(unix)]
    struct FakeGit {
        _lock: MutexGuard<'static, ()>,
        old_path: Option<std::ffi::OsString>,
        _bin: TempDir,
    }

    #[cfg(unix)]
    impl Drop for FakeGit {
        fn drop(&mut self) {
            match &self.old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[cfg(unix)]
    fn fake_git(script: &str) -> FakeGit {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::config::lock_env();
        let bin = TempDir::new().unwrap();
        let old_path = std::env::var_os("PATH");
        let path = bin.path().join("git");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut new_path = bin.path().as_os_str().to_os_string();
        if let Some(old) = &old_path {
            new_path.push(":");
            new_path.push(old);
        }
        std::env::set_var("PATH", new_path);
        FakeGit {
            _lock,
            old_path,
            _bin: bin,
        }
    }

    /// 挂起型 fake git：记录自身与孙进程 pid，`wait` 挂住不退。
    #[cfg(unix)]
    fn hanging_git(dir: &Path) -> String {
        format!(
            "echo $$ > '{}'; sleep 300 & echo $! > '{}'; wait",
            dir.join("git.pid").display(),
            dir.join("grandchild.pid").display()
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_timeout_kills_hanging_git_and_grandchild() {
        let dir = TempDir::new().unwrap();
        let _fake = fake_git(&hanging_git(dir.path()));
        let dest = TempDir::new().unwrap();

        // 超时取 2s：只放宽 macOS 首次 exec 的启动延迟，超时杀组语义不变。
        let err = clone_git_repo(
            "https://example.invalid/hang.git",
            &dest.path().join("clone"),
            Duration::from_secs(2),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");

        wait_group_gone(
            read_pid(&dir.path().join("git.pid")),
            Some(read_pid(&dir.path().join("grandchild.pid"))),
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_clone_kills_process_group() {
        let dir = TempDir::new().unwrap();
        let _fake = fake_git(&hanging_git(dir.path()));
        let dest = TempDir::new().unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let cancel = token.clone();
        let started = dir.path().join("git.pid");
        std::thread::spawn(move || {
            for _ in 0..200 {
                if started.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            cancel.cancel();
        });
        let err = clone_git_repo(
            "https://example.invalid/hang.git",
            &dest.path().join("clone"),
            Duration::from_secs(60),
            Some(&token),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        wait_group_gone(
            read_pid(&dir.path().join("git.pid")),
            Some(read_pid(&dir.path().join("grandchild.pid"))),
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_output_overflow_kills_group_and_bounds_error() {
        let dir = TempDir::new().unwrap();
        let _fake = fake_git(&format!(
            "echo $$ > '{}'; yes fake-git-flood-0123456789",
            dir.path().join("git.pid").display()
        ));
        let dest = TempDir::new().unwrap();

        let err = clone_git_repo(
            "https://example.invalid/flood.git",
            &dest.path().join("clone"),
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("failed to clone"), "{message}");
        assert!(message.contains("truncated"), "{message}");
        assert!(
            message.len() < GIT_OUTPUT_CAP_BYTES / 2,
            "error echo must stay bounded, got {} bytes",
            message.len()
        );
        wait_group_gone(read_pid(&dir.path().join("git.pid")), None).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_failure_error_redacts_credentials_from_git_output() {
        let url = "https://user:sekrittok@gitlab.example/x/y.git";
        let _fake = fake_git(&format!(
            "echo \"fatal: Authentication failed for '{url}/'\" >&2; exit 128"
        ));
        let dest = TempDir::new().unwrap();

        let err = clone_git_repo(
            url,
            &dest.path().join("clone"),
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("failed to clone"), "{message}");
        assert!(!message.contains("sekrittok"), "{message}");
        assert!(message.contains("<git-url>"), "{message}");
    }
}
