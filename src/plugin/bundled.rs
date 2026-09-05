//! bundled 插件：`include_dir` 内嵌仓库 `bundled/` 目录（第三版 §1）。
//!
//! 内核与 bundled 的关系同外部插件：同一份 `04` 校验、同一套启用判定，
//! 只是来源为编译期内嵌而非文件系统目录。加载时先把内嵌树物化到
//! `<data_dir>/bundled/` 缓存父目录下（`Plugin.root` 需要真实路径给组件
//! 运行时用），再走 [`read_manifest`]。bundled 恒为最低优先级：同名插件
//! 一律被外部发现的结果覆盖（第三版 §1"用户插件优先"）。
//! bundled provider 定义不使用 `${PLUGIN_ROOT}`（无稳定文件系统 root）。
//!
//! ## 缓存布局（todo 06 / I03：完整不可变快照）
//!
//! `<data_dir>/bundled/` 是缓存父目录，按内嵌内容集合发布一份经过完整性
//! 核验的不可变快照，不原地逐文件覆盖正在被读取的共同根目录：
//!
//! ```text
//! <data_dir>/bundled/
//! ├── v1-<fnv1a64>/                  # 完整快照 = Plugin.root（文件集合/内容逐字节等于内嵌）
//! │   ├── plugin.json
//! │   └── dev.instagent/providers/*.json
//! ├── .staging-v1-<id>-<pid>-<uuid>/ # 私有临时目录：写满并读回核验后原子 rename 发布；失败即删除
//! ├── .retired-v1-<id>-<pid>-<uuid>/ # 被整体替换的损坏快照（替换成功后尽力删除）
//! └── <旧布局文件/其他目录>            # 历史残留：保留在盘上，但永远不进入运行时发现
//! ```
//!
//! 快照身份 `v1-<fnv1a64>` 由全部内嵌文件的"相对路径 + 内容"（按路径排序）
//! 做 FNV-1a 64 得到：仅作缓存匹配的身份，不是密码学承诺；不能只凭
//! `manifest.version` 判有效，资源改变可能没有 bump 版本。
//! 文件集合或内容与内嵌不完全一致（缺失 / 多余 / 被改 / 半份）的目录一律
//! 视为损坏：整体生成完整替代发布，调用方不会读到半份写入；并发发布同一
//! 快照时通过原子 rename 竞争，败者复用已核验完成的目录。失败路径只清理
//! 本次创建的 `.staging-*` / `.retired-*`，不删除其他快照或未知目录。

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
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

/// 快照布局版本：身份前缀。未来布局变化只需换前缀即可让旧缓存失效。
const SNAPSHOT_VERSION: &str = "v1";

/// 发布竞争（rename 竞态 / 损坏目录替换）的最大重试次数。
const MAX_PUBLISH_ATTEMPTS: u32 = 8;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 全部内嵌文件（相对路径 + 内容），按路径排序保证散列确定性。
fn embedded_files() -> Vec<(&'static Path, &'static [u8])> {
    fn collect(dir: &'static Dir<'static>, out: &mut Vec<(&'static Path, &'static [u8])>) {
        for entry in dir.entries() {
            match entry {
                DirEntry::File(file) => out.push((file.path(), file.contents())),
                DirEntry::Dir(sub) => collect(sub, out),
            }
        }
    }
    let mut out = Vec::new();
    collect(&BUNDLED, &mut out);
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

/// 快照身份：FNV-1a 64 散列全部内嵌"路径 + 内容"。只用于缓存匹配，
/// 不作密码学承诺（todo 06）。
fn snapshot_id() -> String {
    let mut hash = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    for (path, contents) in embedded_files() {
        feed(path.to_string_lossy().as_bytes());
        feed(&[0xff]);
        feed(&(contents.len() as u64).to_le_bytes());
        feed(&[0xfe]);
        feed(contents);
        feed(&[0xfd]);
    }
    format!("{SNAPSHOT_VERSION}-{hash:016x}")
}

/// 物化内嵌树并返回快照根目录。以 `<base>/bundled/` 为缓存父目录：
/// 已存在与内嵌内容一致的快照时直接复用（一个字节都不重写）；否则在私有
/// 临时目录写满并读回核验后原子发布；损坏快照被完整替代（布局与并发语义
/// 见模块文档）。数据根目录由参数给出（同 `install.rs`，测试不改写进程
/// 全局 `INSTAGENT_DATA_DIR`，避免与 `session.rs` 测试互踩）。
pub(crate) fn materialize_at(base: &Path) -> crate::Result<PathBuf> {
    let parent = base.join(BUNDLED_PLUGIN_NAME);
    std::fs::create_dir_all(&parent)?;
    let target = parent.join(snapshot_id());
    if validate_snapshot(&target) {
        return Ok(target);
    }
    build_snapshot(&parent, &target)
}

/// 生成并发布完整快照：并发竞态时复用已发布的一方；失败只清理本次
/// 创建的临时目录，不删除旧快照、未知目录或正在使用的内容。
fn build_snapshot(parent: &Path, target: &Path) -> crate::Result<PathBuf> {
    let mut last_error = None;
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        if validate_snapshot(target) {
            return Ok(target.to_path_buf());
        }
        let staging = unique_hidden_path(parent, target, ".staging");
        if let Err(err) = write_staging(&staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(err)
                .with_context(|| format!("stage bundled snapshot {}", staging.display()));
        }
        #[cfg(test)]
        if TEST_FAIL_PUBLISH.with(std::cell::Cell::get) {
            let _ = std::fs::remove_dir_all(&staging);
            bail!("injected bundled snapshot publish failure (test hook)");
        }
        match publish_once(parent, &staging, target) {
            Ok(root) => return Ok(root),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "bundled snapshot publish: no attempts made for {}",
            target.display()
        )
    }))
}

/// 私有临时目录名：点前缀 + 快照名 + pid + uuid，并发互不碰撞，失败回收
/// 时能明确识别"本次创建的"。
fn unique_hidden_path(parent: &Path, target: &Path, prefix: &str) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(BUNDLED_PLUGIN_NAME);
    parent.join(format!(
        "{prefix}-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// 完整写入私有临时目录 + 读回核验：文件集合与内容必须与内嵌完全一致
/// 才允许发布（调用方永远不读半份目录）。
fn write_staging(staging: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(staging)?;
    #[cfg(test)]
    let mut written = 0usize;
    for (rel, contents) in embedded_files() {
        let dst = staging.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, contents)?;
        #[cfg(test)]
        {
            written += 1;
            if TEST_FAIL_WRITE.with(std::cell::Cell::get) && written == 1 {
                bail!("injected bundled snapshot write failure (test hook)");
            }
        }
    }
    check_snapshot(staging)
        .with_context(|| format!("verify staged bundled snapshot {}", staging.display()))
}

/// 把 staging 原子发布为 target。竞争失败（rename 败北 / target 被并发
/// 移走）时清理本次的 staging 并返回 Err，由调用方重试复用胜者；绝不
/// 原地覆盖正在被读取的目录。
fn publish_once(parent: &Path, staging: &Path, target: &Path) -> crate::Result<PathBuf> {
    if !target.exists() {
        if let Err(err) = std::fs::rename(staging, target) {
            let _ = std::fs::remove_dir_all(staging);
            return Err(err).with_context(|| format!("publish {}", target.display()));
        }
        return Ok(target.to_path_buf());
    }
    // target 存在但没通过外层校验：可能并发发布刚完成，先复验，
    // 避免误把刚发布的有效快照移走。
    if validate_snapshot(target) {
        let _ = std::fs::remove_dir_all(staging);
        return Ok(target.to_path_buf());
    }
    // 损坏 target：先移到旁边再替换（已打开文件的读者不受影响），
    // 绝不原地覆盖正在被读取的目录。
    let retired = unique_hidden_path(parent, target, ".retired");
    if let Err(err) = std::fs::rename(target, &retired) {
        let _ = std::fs::remove_dir_all(staging);
        return Err(err).with_context(|| format!("retire corrupted {}", target.display()));
    }
    match std::fs::rename(staging, target) {
        Ok(()) => {
            // retired 是本次核验失败并移走的内容，尽力删除；删不掉
            //（如仍有句柄打开）也不影响发现——它不在返回路径上。
            let _ = std::fs::remove_dir_all(&retired);
            Ok(target.to_path_buf())
        }
        Err(err) => {
            // 移走期间并发进程重建了 target：复用胜者，清理本次产物。
            let _ = std::fs::remove_dir_all(staging);
            let _ = std::fs::remove_dir_all(&retired);
            if validate_snapshot(target) {
                return Ok(target.to_path_buf());
            }
            Err(err).with_context(|| format!("publish {}", target.display()))
        }
    }
}

/// 快照完整性：文件集合与内嵌完全一致（无缺失 / 无多余）且内容逐字节
/// 相等。任何不一致（含不存在、不可读、符号链接、半份写入）都算损坏。
fn validate_snapshot(root: &Path) -> bool {
    check_snapshot(root).is_ok()
}

fn check_snapshot(root: &Path) -> crate::Result<()> {
    let mut on_disk = Vec::new();
    collect_relative_files(root, root, &mut on_disk)?;
    on_disk.sort();
    let embedded = embedded_files();
    let expected: Vec<PathBuf> = embedded.iter().map(|(rel, _)| rel.to_path_buf()).collect();
    if on_disk != expected {
        bail!(
            "bundled snapshot file set mismatch ({} files on disk, {} embedded)",
            on_disk.len(),
            expected.len()
        );
    }
    for (rel, contents) in embedded.iter().copied() {
        let path = root.join(rel);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes != contents {
            bail!("bundled snapshot content mismatch: {}", path.display());
        }
    }
    Ok(())
}

/// 递归收集 root 下常规文件的相对路径；符号链接与特殊文件一律拒绝
///（不能让盘上的异常条目冒充内嵌内容）。
fn collect_relative_files(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let file_type = std::fs::symlink_metadata(&path)?.file_type();
        if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported entry in bundled snapshot: {}", path.display()),
            ));
        }
        if file_type.is_dir() {
            collect_relative_files(base, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}

// 测试钩子：注入写 / 发布失败，验证失败回收与旧快照保护。
#[cfg(test)]
thread_local! {
    static TEST_FAIL_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_FAIL_PUBLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
/// 补在最低优先级（合并规则见 `with_bundled`）。
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

/// 纯合并：外部任一层已有同名 `bundled` 时不注入；启用判定与 `05` 共用
/// [`plugin_enabled`]（白名单含显式 `[]` = 禁用全部，未表态才看 disabled）。
fn with_bundled(mut set: PluginSet, bundled: Plugin, settings: &Settings) -> PluginSet {
    let name = &bundled.manifest.name;
    if set.get(name).is_some() {
        return set;
    }
    if crate::plugin::plugin_enabled(name, settings) {
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

    fn dir_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn assert_dir_names(dir: &Path, expected: &[&str]) {
        let mut names = dir_names(dir);
        names.sort();
        let mut expected: Vec<String> = expected.iter().map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(names, expected, "entries under {}", dir.display());
    }

    /// 预置一份"完整内嵌集合"（供损坏用例在其上做手脚）。
    fn seed_full_snapshot(dir: &Path) {
        for (rel, contents) in embedded_files() {
            let dst = dir.join(rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(dst, contents).unwrap();
        }
    }

    #[test]
    fn materializes_and_loads_bundled_manifest() {
        let env = isolated();
        let plugin = load_bundled_at(env.data.path()).unwrap();
        assert_eq!(plugin.manifest.name, BUNDLED_PLUGIN_NAME);
        assert_eq!(plugin.source, PluginSource::Bundled);
        assert!(plugin.root.join("plugin.json").is_file());
        let parent = env.data.path().join(BUNDLED_PLUGIN_NAME);
        assert_eq!(
            plugin.root,
            parent.join(snapshot_id()),
            "root 指向缓存父目录下的完整快照"
        );
        check_snapshot(&plugin.root).unwrap();
    }

    /// T1：返回根的文件集合与嵌入集合一致，内容逐字节一致。
    #[test]
    fn snapshot_matches_embedded_set_and_bytes() {
        let env = isolated();
        let root = materialize_at(env.data.path()).unwrap();
        let mut on_disk = Vec::new();
        collect_relative_files(&root, &root, &mut on_disk).unwrap();
        on_disk.sort();
        let embedded = embedded_files();
        let expected: Vec<PathBuf> = embedded.iter().map(|(rel, _)| rel.to_path_buf()).collect();
        assert_eq!(on_disk, expected, "文件集合必须与内嵌完全一致");
        for (rel, contents) in embedded.iter().copied() {
            let bytes = std::fs::read(root.join(rel)).unwrap();
            assert_eq!(bytes, contents, "{} 必须逐字节一致", rel.display());
        }
        assert_eq!(read_manifest(&root).unwrap().name, BUNDLED_PLUGIN_NAME);
    }

    /// T1：旧 `<base>/bundled/` 平铺布局（含旧 ghost provider、旧 hooks）
    /// 保留在盘上，但不进入新 `Plugin.root`，也不被 provider 发现加载。
    #[test]
    fn legacy_layout_files_remain_but_are_not_loaded() {
        let env = isolated();
        let parent = env.data.path().join(BUNDLED_PLUGIN_NAME);
        let legacy_providers = parent.join(crate::plugin::NAMESPACE).join("providers");
        let legacy_hooks = parent.join(crate::plugin::NAMESPACE).join("hooks");
        std::fs::create_dir_all(&legacy_providers).unwrap();
        std::fs::create_dir_all(&legacy_hooks).unwrap();
        std::fs::write(parent.join("plugin.json"), r#"{"name":"legacy"}"#).unwrap();
        std::fs::write(legacy_providers.join("ghost.json"), r#"{"name":"ghost"}"#).unwrap();
        std::fs::write(legacy_hooks.join("old.md"), "legacy hook").unwrap();

        let plugin = load_bundled_at(env.data.path()).unwrap();
        assert_eq!(plugin.root, parent.join(snapshot_id()));
        // 旧布局保留在缓存父目录（失败回收不删未知内容）。
        assert!(parent.join("plugin.json").is_file());
        assert!(legacy_providers.join("ghost.json").is_file());
        // 新根只含内嵌集合：逐文件核验，ghost/旧 hooks 不在。
        check_snapshot(&plugin.root).unwrap();
        assert!(!plugin
            .root
            .join(crate::plugin::NAMESPACE)
            .join("providers")
            .join("ghost.json")
            .exists());
        // 运行时发现（provider 装配）只见内嵌的 5 个 provider。
        let set = PluginSet {
            plugins: vec![plugin],
            skipped: vec![],
        };
        let registry =
            crate::provider::ProviderRegistry::from_plugins_at(&set, env.data.path()).unwrap();
        let listed = registry.names();
        let provider_names: Vec<&str> = listed.iter().map(String::as_str).collect();
        assert_eq!(
            provider_names,
            ["deepseek", "groq", "ollama", "openai", "openrouter"]
        );
    }

    /// T2：重复正常加载不重写有效文件（复用已核验快照，无临时残留）。
    #[test]
    fn repeat_load_reuses_snapshot_without_rewriting() {
        let env = isolated();
        let root = materialize_at(env.data.path()).unwrap();
        let probe = root
            .join(crate::plugin::NAMESPACE)
            .join("providers")
            .join("openai.json");
        let manifest_path = root.join("plugin.json");
        let probe_before = std::fs::metadata(&probe).unwrap().modified().unwrap();
        let manifest_before = std::fs::metadata(&manifest_path)
            .unwrap()
            .modified()
            .unwrap();

        for _ in 0..3 {
            let plugin = load_bundled_at(env.data.path()).unwrap();
            assert_eq!(plugin.root, root);
        }
        assert_eq!(
            std::fs::metadata(&probe).unwrap().modified().unwrap(),
            probe_before,
            "有效文件不得被重写"
        );
        assert_eq!(
            std::fs::metadata(&manifest_path)
                .unwrap()
                .modified()
                .unwrap(),
            manifest_before,
            "有效文件不得被重写"
        );
        let parent = env.data.path().join(BUNDLED_PLUGIN_NAME);
        assert_dir_names(&parent, &[snapshot_id().as_str()]);
    }

    /// T2：预置半份 JSON、额外文件、被修改的合法 JSON 均不能冒充正确快照；
    /// 修复后的根必须通过逐字节集合核验（不只检查目录存在）。
    #[test]
    fn corrupted_snapshots_are_fully_replaced() {
        let _env = isolated();
        let id = snapshot_id();

        // a) 半份 JSON + 缺失文件：整体重建为完整集合。
        let base = TempDir::new().unwrap();
        let target = base.path().join(BUNDLED_PLUGIN_NAME).join(&id);
        let embedded = embedded_files();
        let (first_rel, first_contents) = embedded[0];
        std::fs::create_dir_all(target.join("dev.instagent/providers")).unwrap();
        std::fs::write(
            target.join(first_rel),
            &first_contents[..first_contents.len() / 2],
        )
        .unwrap();
        let root = materialize_at(base.path()).unwrap();
        assert_eq!(root, target);
        check_snapshot(&root).unwrap();
        assert_eq!(read_manifest(&root).unwrap().name, BUNDLED_PLUGIN_NAME);

        // b) 完整集合 + 额外 ghost provider：ghost 不得留在新快照里。
        let base = TempDir::new().unwrap();
        let target = base.path().join(BUNDLED_PLUGIN_NAME).join(&id);
        seed_full_snapshot(&target);
        let ghost = target
            .join(crate::plugin::NAMESPACE)
            .join("providers")
            .join("ghost.json");
        std::fs::write(&ghost, r#"{"name":"ghost","engine":"openai"}"#).unwrap();
        let root = materialize_at(base.path()).unwrap();
        check_snapshot(&root).unwrap();
        assert!(!root
            .join(crate::plugin::NAMESPACE)
            .join("providers")
            .join("ghost.json")
            .exists());

        // c) 被修改的合法 JSON（manifest.version 未变）：内容变了就必须重建，
        //    不能只凭版本判缓存有效。
        let base = TempDir::new().unwrap();
        let target = base.path().join(BUNDLED_PLUGIN_NAME).join(&id);
        seed_full_snapshot(&target);
        let openai = target
            .join(crate::plugin::NAMESPACE)
            .join("providers")
            .join("openai.json");
        let mut tampered: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&openai).unwrap()).unwrap();
        tampered["display_name"] = serde_json::Value::String("Tampered".into());
        std::fs::write(&openai, tampered.to_string()).unwrap();
        let root = materialize_at(base.path()).unwrap();
        check_snapshot(&root).unwrap();
        let (rel, contents) = embedded
            .iter()
            .copied()
            .find(|(rel, _)| rel.ends_with("providers/openai.json"))
            .unwrap();
        assert_eq!(std::fs::read(root.join(rel)).unwrap(), contents);
    }

    /// T2：并发发布相同快照复用已核验目录；结束后父目录只有那一份快照，
    /// 每个线程每次读到的都是完整集合（不见半份写入）。
    #[test]
    fn concurrent_materialize_publishes_one_snapshot() {
        let _env = isolated();
        let base = TempDir::new().unwrap();
        let base_path = base.path().to_path_buf();
        let expected = base.path().join(BUNDLED_PLUGIN_NAME).join(snapshot_id());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let base = base_path.clone();
            handles.push(std::thread::spawn(move || {
                let mut roots = Vec::new();
                for _ in 0..5 {
                    let root = materialize_at(&base).expect("并发 materialize 必须成功");
                    check_snapshot(&root).expect("并发读取不得见到半份快照");
                    read_manifest(&root).expect("manifest 可读");
                    roots.push(root);
                }
                roots
            }));
        }
        for handle in handles {
            let roots = handle.join().unwrap();
            assert!(roots.iter().all(|root| root == &expected), "{roots:?}");
        }
        check_snapshot(&expected).unwrap();
        assert_dir_names(expected.parent().unwrap(), &[snapshot_id().as_str()]);
    }

    /// T2 多进程入口：由 `multi_process_materialize_and_reads_succeed` 经
    /// `current_exe()` + `--exact` 在子进程里单独运行本用例，与兄弟进程和
    /// 父进程竞态；常规运行（无环境变量）直接 no-op。
    #[test]
    fn child_materialize_and_read_loop() {
        let Some(base) = std::env::var_os("INSTAGENT_BUNDLED_TEST_BASE") else {
            return;
        };
        let base = PathBuf::from(base);
        for _ in 0..6 {
            let root = materialize_at(&base).expect("child materialize");
            let manifest = read_manifest(&root).expect("child manifest read");
            assert_eq!(manifest.name, BUNDLED_PLUGIN_NAME);
            check_snapshot(&root).expect("child 不得见到半份快照");
            let providers = root.join(crate::plugin::NAMESPACE).join("providers");
            let mut count = 0usize;
            for entry in std::fs::read_dir(&providers).expect("providers dir") {
                let path = entry.unwrap().path();
                let text = std::fs::read_to_string(&path).expect("provider json read");
                let value: serde_json::Value =
                    serde_json::from_str(&text).expect("provider json parse");
                assert!(value["name"].is_string(), "{path:?}");
                count += 1;
            }
            let expected = embedded_files()
                .iter()
                .filter(|(rel, _)| rel.starts_with(crate::plugin::NAMESPACE))
                .count();
            assert_eq!(count, expected, "provider 数量");
        }
    }

    /// T2：同一 base 上多线程 + 多进程 materialize 与 manifest/provider
    /// 读取始终成功；子进程经进程组 + `kill_on_drop(true)` 启动。
    #[tokio::test]
    async fn multi_process_materialize_and_reads_succeed() {
        let _env = isolated();
        let base = TempDir::new().unwrap();
        let base_path = base.path().to_path_buf();

        let mut children = Vec::new();
        for _ in 0..3 {
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("plugin::bundled::tests::child_materialize_and_read_loop")
                .arg("--exact")
                .env("INSTAGENT_BUNDLED_TEST_BASE", &base_path);
            crate::subprocess::configure_subprocess(&mut command);
            children.push(command.spawn().expect("spawn child materializer"));
        }
        // 父进程内线程同时参与竞态。
        let writer_base = base_path.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..6 {
                let root = materialize_at(&writer_base).expect("parent materialize");
                check_snapshot(&root).expect("parent 读取不得见到半份快照");
                read_manifest(&root).expect("parent manifest read");
            }
        });

        for child in children {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                child.wait_with_output(),
            )
            .await
            .expect("child timed out")
            .expect("child wait");
            assert!(
                output.status.success(),
                "child materialize loop failed: {}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        writer.join().unwrap();

        let expected = base.path().join(BUNDLED_PLUGIN_NAME).join(snapshot_id());
        check_snapshot(&expected).unwrap();
        assert_dir_names(expected.parent().unwrap(), &[snapshot_id().as_str()]);
    }

    /// T2：注入写 / 发布失败后，旧快照可读且父目录不遗留本次的临时目录；
    /// 恢复后正常发布，旧目录仍不被触碰。
    #[test]
    fn injected_failures_keep_old_snapshot_and_leave_no_tmp() {
        let _env = isolated();
        let base = TempDir::new().unwrap();
        let parent = base.path().join(BUNDLED_PLUGIN_NAME);
        std::fs::create_dir_all(&parent).unwrap();
        // 模拟旧版本残留：当前身份缺失，只有一个未知旧目录。
        let old = parent.join("v1-0000000000000000");
        std::fs::create_dir_all(old.join("dev.instagent/providers")).unwrap();
        std::fs::write(old.join("plugin.json"), r#"{"name":"old-generation"}"#).unwrap();
        std::fs::write(
            old.join("dev.instagent/providers").join("old.json"),
            r#"{"name":"old"}"#,
        )
        .unwrap();
        let old_probe = old.join("plugin.json");
        let old_bytes = std::fs::read(&old_probe).unwrap();
        let old_name = "v1-0000000000000000";

        // 写失败：半份临时目录被回收，旧快照原样可读。
        TEST_FAIL_WRITE.with(|flag| flag.set(true));
        assert!(materialize_at(base.path()).is_err());
        TEST_FAIL_WRITE.with(|flag| flag.set(false));
        assert_eq!(std::fs::read(&old_probe).unwrap(), old_bytes);
        assert_dir_names(&parent, &[old_name]);

        // 发布失败：同样不遗留本次 tmp。
        TEST_FAIL_PUBLISH.with(|flag| flag.set(true));
        assert!(materialize_at(base.path()).is_err());
        TEST_FAIL_PUBLISH.with(|flag| flag.set(false));
        assert_eq!(std::fs::read(&old_probe).unwrap(), old_bytes);
        assert_dir_names(&parent, &[old_name]);

        // 故障后恢复：正常发布成功，旧目录继续保留且可读。
        let root = materialize_at(base.path()).unwrap();
        check_snapshot(&root).unwrap();
        assert_eq!(std::fs::read(&old_probe).unwrap(), old_bytes);
        assert_dir_names(&parent, &[old_name, snapshot_id().as_str()]);
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
