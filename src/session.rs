//! 会话：JSONL，不用 sqlite（第二版 §2.6）。
//!
//! 路径 `<data_dir>/sessions/<id>.jsonl`；data_dir 用 etcetera，默认
//! `~/.local/share/instagent`，可被 `INSTAGENT_DATA_DIR` 覆盖（测试用）。
//! 第 1 行 header，之后每行一条 Message。
//!
//! 边界与持久化契约（plan S1-S4/R10、ADR 0003 D6 承诺的应用层纵深防御）：
//! - 任意外部 session id 先过 [`validate_session_id`]（安全文件名白名单），
//!   拒绝空串、`.` / `..`、绝对路径、路径分隔符与非安全字符；
//!   create / resume / open_or_resume / remove / list 全部复用，保证 CLI 传入
//!   的 id 不会读/删 sessions 根目录外的文件。sessions 目录内的 symlink 主
//!   文件也拒绝打开（不跟随）。
//! - Unix：sessions 目录 0700；会话主文件 / 临时文件 / 备份 0600。
//! - durability：append 至少 flush（进程崩溃不丢，主机掉电不保证）；create /
//!   rewrite / salvage 修复写回额外 `sync_all`，rename 后尽力同步父目录。
//! - 原子重写：header+消息写随机后缀私有临时文件，旧主文件先复制为带时间戳
//!   的 `<id>.<ts>.bak.jsonl` 备份（best-effort，每会话保留最近
//!   `MAX_BACKUPS` 份），再 rename 覆盖主文件；rename 失败清理临时文件。
//!   不变量：主文件任何时刻要么是旧内容要么是新内容，绝不缺失。
//! - resume 的 salvage（坏行截断 + 合法前缀回退）会把有效前缀**物理写回**
//!   主 JSONL，保证 resume→append→resume 不再重复丢尾部（S2）。
//! - list / `last` 对单个坏文件只输出带路径的诊断并跳过，不整体失败；
//!   `last` 只在可验证会话中按 mtime 选最新，目录里有文件但全部不可验证时
//!   报诊断错误而非静默新建（S3）。

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::bail;
use anyhow::Context;
use etcetera::choose_app_strategy;
use etcetera::AppStrategy;
use etcetera::AppStrategyArgs;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::message;
use crate::message::Message;

const SESSIONS_SUBDIR: &str = "sessions";
const JSONL_EXT: &str = "jsonl";
const BACKUP_SUFFIX: &str = ".bak.jsonl";
const TMP_SUFFIX: &str = ".jsonl.tmp";
/// 每个会话最多保留的 `.bak.jsonl` 份数（R10：备份有界）。
const MAX_BACKUPS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub id: String,
    /// epoch 秒。
    pub created: i64,
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub header: SessionHeader,
    pub messages: Vec<Message>,
    /// `<data_dir>/sessions/<id>.jsonl`
    pub path: PathBuf,
}

impl Session {
    /// `<data_dir>/sessions/`，data_dir：`INSTAGENT_DATA_DIR` 环境变量优先，
    /// 否则 etcetera（XDG）的 `~/.local/share/instagent`。
    pub fn sessions_dir() -> crate::Result<PathBuf> {
        let data_dir = match std::env::var_os("INSTAGENT_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let args = AppStrategyArgs {
                    top_level_domain: "dev".to_string(),
                    author: "instagent".to_string(),
                    app_name: "instagent".to_string(),
                };
                choose_app_strategy(args)
                    .context("resolve data dir via etcetera")?
                    .data_dir()
            }
        };
        Ok(data_dir.join(SESSIONS_SUBDIR))
    }

    /// 新建会话（uuid v4，写 header 行）。sessions 目录 0700、文件 0600
    /// （unix）；header 落盘 sync。
    pub fn create(cwd: &Path, provider: &str, model: &str) -> crate::Result<Session> {
        let dir = Self::sessions_dir()?;
        ensure_sessions_dir(&dir)?;
        let header = SessionHeader {
            id: Uuid::new_v4().to_string(),
            created: crate::message::now_ts(),
            cwd: cwd.to_path_buf(),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let path = session_path(&dir, &header.id)?;
        let mut file = create_private(&path)
            .with_context(|| format!("create session file {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        file.flush()?;
        file.sync_all()?;
        Ok(Session {
            header,
            messages: Vec::new(),
            path,
        })
    }

    /// 全量读回。id 先过 [`validate_session_id`]。header（第 1 行）损坏直接
    /// 报错带行号；正文按"合法前缀"恢复：遇到坏行即截断，再回退到最近一条
    /// 满足 `message::validate` 的前缀，丢弃行数向 stderr 警告；只剩 header
    /// 时按空会话打开。有丢弃时把有效前缀**原子写回主 JSONL**（旧内容存
    /// 时间戳备份）——否则之后的 append 落在坏行之后，下次 resume 会再次
    /// 丢弃同一尾部，造成永久数据丢失（S2）。
    pub fn resume(id: &str) -> crate::Result<Session> {
        let path = session_path(&Self::sessions_dir()?, id)?;
        let header = load_header(&path)?;
        let file = open_session_file(&path)?;
        let mut lines = BufReader::new(file).lines();
        let _ = next_line(&mut lines, &path, 1)?;
        let mut raws = Vec::new();
        let mut num = 2usize;
        while let Some(raw) = opt_line(&mut lines, &path, num)? {
            raws.push(raw);
            num += 1;
        }
        let (messages, dropped) = salvage(&raws);
        if dropped > 0 {
            eprintln!(
                "warning: session {}: dropped {dropped} unrecoverable line(s), kept {} message(s)",
                path.display(),
                messages.len()
            );
            atomic_replace(&path, &header, &messages)
                .with_context(|| format!("repair salvaged session {}", path.display()))?;
        }
        Ok(Session {
            header,
            messages,
            path,
        })
    }

    /// `id` 为 None 新建；Some("last") 取最近（按文件 mtime）**且可验证**的
    /// 会话——坏文件只产生带路径的 warning 并被跳过，不遮蔽有效会话；目录
    /// 里有文件但全部不可验证时报诊断错误而非静默新建；否则按 id。
    pub fn open_or_resume(
        id: Option<&str>,
        cwd: &Path,
        provider: &str,
        model: &str,
    ) -> crate::Result<Session> {
        match id {
            None => Self::create(cwd, provider, model),
            Some("last") => {
                let dir = Self::sessions_dir()?;
                let files = session_files(&dir)?;
                let mut newest: Option<(std::time::SystemTime, SessionHeader)> = None;
                for path in &files {
                    let modified = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .unwrap_or(UNIX_EPOCH);
                    match load_header(path) {
                        Ok(header) => {
                            if newest.as_ref().is_none_or(|(m, _)| modified > *m) {
                                newest = Some((modified, header));
                            }
                        }
                        Err(err) => eprintln!("warning: {err:#}, skipping"),
                    }
                }
                match newest {
                    Some((_, header)) => Self::resume(&header.id),
                    None if files.is_empty() => Self::create(cwd, provider, model),
                    None => bail!(
                        "no valid session to resume: {} session file(s) unreadable (see warnings above)",
                        files.len()
                    ),
                }
            }
            Some(id) => Self::resume(id),
        }
    }

    /// 统一消息边界（S8）：先经 `message::validate_for_append`（与 provider
    /// wire / rewrite 同一校验核心，仅容忍"尾部 assistant 的 tool calls 尚未
    /// 答复"这一 `finish_turn` 中间态）再落盘；校验失败不落盘、不入库，错误
    /// 只带索引与会话 id，不回显消息原文。
    ///
    /// 追加一行 + 立即 flush（最低 durability：进程崩溃不丢；主机掉电不
    /// 保证——create / rewrite / salvage 修复路径才 `sync_all`）。
    pub fn append(&mut self, message: Message) -> crate::Result<()> {
        let index = self.messages.len();
        self.messages.push(message);
        let written = (|| -> crate::Result<()> {
            message::validate_for_append(&self.messages)?;
            let mut file = OpenOptions::new()
                .append(true)
                .open(&self.path)
                .with_context(|| format!("open session file for append {}", self.path.display()))?;
            writeln!(file, "{}", serde_json::to_string(&self.messages[index])?)?;
            file.flush()?;
            Ok(())
        })();
        if written.is_err() {
            self.messages.pop();
        }
        written.with_context(|| {
            format!(
                "invalid append at message {index} in session {}",
                self.header.id
            )
        })
    }

    /// 只读每个文件的第一行（header），按 created 降序。单个坏 header/坏
    /// 文件只输出带路径的 warning 并跳过，不让整体失败。
    pub fn list() -> crate::Result<Vec<SessionHeader>> {
        let dir = Self::sessions_dir()?;
        let mut headers = Vec::new();
        for path in session_files(&dir)? {
            match load_header(&path) {
                Ok(header) => headers.push(header),
                Err(err) => eprintln!("warning: {err:#}, skipping"),
            }
        }
        headers.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        Ok(headers)
    }

    /// 删除会话文件（`sessions rm`）。id 先过校验，杜绝越出 sessions 根目录。
    pub fn remove(id: &str) -> crate::Result<()> {
        let path = session_path(&Self::sessions_dir()?, id)?;
        fs::remove_file(&path).with_context(|| format!("remove session file {}", path.display()))
    }

    /// 压缩用原子重写（`16` 调用）：委托 `atomic_replace`（私有临时文件 +
    /// sync + 时间戳备份 + rename + 失败清理 + 父目录同步）。
    pub fn rewrite(&mut self, messages: Vec<Message>) -> crate::Result<()> {
        message::validate(&messages)
            .with_context(|| format!("validate rewritten session {}", self.header.id))?;
        atomic_replace(&self.path, &self.header, &messages)?;
        self.messages = messages;
        Ok(())
    }
}

/// session id 安全文件名白名单（S1 / ADR 0003 D6）：只允许 UUID 形态的字符
/// （ASCII 字母数字与 `-` `_` `.`），拒绝空串、`.` / `..`、绝对路径、路径
/// 分隔符（`/` `\`）与其它非安全字符；校验通过后 `join` 出的路径必然落在
/// sessions 根目录内。
pub fn validate_session_id(id: &str) -> crate::Result<()> {
    let safe = !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if safe {
        Ok(())
    } else {
        bail!(
            "invalid session id {id:?}: expected a UUID-like safe file name \
             (no absolute paths, parent components or separators)"
        )
    }
}

/// 校验 id 并拼出 `<dir>/<id>.jsonl`。
fn session_path(dir: &Path, id: &str) -> crate::Result<PathBuf> {
    validate_session_id(id)?;
    Ok(dir.join(format!("{id}.{JSONL_EXT}")))
}

/// 建 sessions 目录：unix 0700（0700 无 group/other 位，umask 不会变暗；
/// 已存在的旧 0755 目录也收紧）。
fn ensure_sessions_dir(dir: &Path) -> crate::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("create sessions dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// 创建私有（unix 0600）新文件：header、临时文件共用。
fn create_private(path: &Path) -> crate::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

/// 打开主会话文件读取；拒绝 symlink（不把 sessions 根目录外的文件吸进来）。
fn open_session_file(path: &Path) -> crate::Result<File> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("open session file {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("refusing to open symlinked session file {}", path.display());
    }
    File::open(path).with_context(|| format!("open session file {}", path.display()))
}

/// 读取并校验单个会话主文件（list / `last` 的"可验证"判定）：stem 是合法
/// id、主文件不是 symlink、header 行可解析且 header.id 与文件名一致。
/// 错误信息自带文件路径。
fn load_header(path: &Path) -> crate::Result<SessionHeader> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("session file name not valid UTF-8: {}", path.display()))?;
    validate_session_id(id).with_context(|| format!("bad session file {}", path.display()))?;
    let file = open_session_file(path)?;
    let mut lines = BufReader::new(file).lines();
    let raw = next_line(&mut lines, path, 1)?;
    let header: SessionHeader = parse_line(&raw, path, 1)?;
    if header.id != id {
        bail!(
            "session {}: header id {:?} != file name {id:?}",
            path.display(),
            header.id
        );
    }
    Ok(header)
}

/// 原子重写主 JSONL（rewrite 与 salvage 修复共用）：header+消息写随机后缀
/// 私有临时文件（flush + sync_all），旧主文件复制为带时间戳备份（best-
/// effort，失败只警告），rename 覆盖主文件；rename 失败清理临时文件并报错；
/// 成功后尽力同步父目录（unix，best-effort）。
fn atomic_replace(path: &Path, header: &SessionHeader, messages: &[Message]) -> crate::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let tmp_path = dir.join(format!("{stem}.{}{TMP_SUFFIX}", Uuid::new_v4()));
    {
        let mut file = create_private(&tmp_path)
            .with_context(|| format!("create temp file {}", tmp_path.display()))?;
        writeln!(file, "{}", serde_json::to_string(header)?)?;
        for message in messages {
            writeln!(file, "{}", serde_json::to_string(message)?)?;
        }
        file.flush()?;
        file.sync_all()?;
    }
    let bak = backup_path(path);
    if let Err(err) = fs::copy(path, &bak) {
        tracing::warn!(
            "session {stem}: backup copy to {} failed: {err}",
            bak.display()
        );
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&bak, fs::Permissions::from_mode(0o600));
        }
        prune_backups(dir, stem);
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| {
            format!(
                "move new session {} to {}",
                tmp_path.display(),
                path.display()
            )
        });
    }
    sync_dir_parent(dir);
    Ok(())
}

/// rename 之后尽力同步父目录，让替换本身落盘（unix；失败忽略）。
fn sync_dir_parent(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(file) = File::open(dir) {
            let _ = file.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// 第一个不占用 `<id>.<epoch 秒>.bak.jsonl` 的名字；同一秒内多次备份退化为
/// `<id>.<ts>.<n>.bak.jsonl`。
fn backup_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let ts = crate::message::now_ts();
    let mut candidate = dir.join(format!("{id}.{ts}{BACKUP_SUFFIX}"));
    let mut n = 1usize;
    while candidate.exists() {
        candidate = dir.join(format!("{id}.{ts}.{n}{BACKUP_SUFFIX}"));
        n += 1;
    }
    candidate
}

/// 每会话保留最近 [`MAX_BACKUPS`] 份备份，更老的删除（best-effort，R10）。
/// ponytail: read_dir + mtime 排序；备份数量上限个位数，够用。
fn prune_backups(dir: &Path, id: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{id}.");
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(BACKUP_SUFFIX))
        })
        .collect();
    if backups.len() <= MAX_BACKUPS {
        return;
    }
    backups.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    for path in backups.iter().take(backups.len() - MAX_BACKUPS) {
        if let Err(err) = fs::remove_file(path) {
            tracing::warn!(
                "session {id}: prune backup {} failed: {err}",
                path.display()
            );
        }
    }
}

/// 会话主文件（排除 `.bak.jsonl` 备份与临时文件）。
fn session_files(dir: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => {
            return Err(err).with_context(|| format!("read sessions dir {}", dir.display()))
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read sessions dir {}", dir.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(BACKUP_SUFFIX) || name.ends_with(TMP_SUFFIX) {
            continue;
        }
        if path.is_file() && name.ends_with(&format!(".{JSONL_EXT}")) {
            files.push(path);
        }
    }
    Ok(files)
}

fn next_line(
    lines: impl Iterator<Item = std::io::Result<String>>,
    path: &Path,
    num: usize,
) -> crate::Result<String> {
    opt_line(lines, path, num)?
        .with_context(|| format!("session file {} ended before line {num}", path.display()))
}

fn opt_line(
    mut lines: impl Iterator<Item = std::io::Result<String>>,
    path: &Path,
    num: usize,
) -> crate::Result<Option<String>> {
    match lines.next() {
        Some(line) => {
            let raw = line.with_context(|| format!("read line {num} of {}", path.display()))?;
            Ok(Some(raw))
        }
        None => Ok(None),
    }
}

fn parse_line<T>(raw: &str, path: &Path, num: usize) -> crate::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(raw)
        .with_context(|| format!("invalid JSON at line {num} of {}: {raw}", path.display()))
}

/// 从正文行（header 之后）恢复最长合法前缀：第一条坏行即截断，再回退到满足
/// `message::validate` 的前缀（如尾部 tool_use 缺结果）。返回（消息，丢弃行数）。
/// ponytail: 回退是 O(n²) 全量 validate；会话规模几百条，够用。
fn salvage(raws: &[String]) -> (Vec<Message>, usize) {
    let mut messages = Vec::new();
    let mut dropped = 0usize;
    for (index, raw) in raws.iter().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(raw) {
            Ok(message) => messages.push(message),
            Err(_) => {
                dropped = raws.len() - index;
                break;
            }
        }
    }
    while !messages.is_empty() && message::validate(&messages).is_err() {
        messages.pop();
        dropped += 1;
    }
    (messages, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Content;
    use crate::message::Role;
    use crate::message::Usage;
    /// 用 tempdir + INSTAGENT_DATA_DIR 隔离数据目录，返回（会话目录守卫）。
    /// 持有返回值期间环境变量有效；串行执行避免测试间竞态。
    /// env 锁统一用 `config::lock_env`（全 crate 一把，见 `19`）。
    fn temp_data_dir() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("INSTAGENT_DATA_DIR", dir.path());
        (dir, guard)
    }

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![Content::Text(text.into())],
            ts: crate::message::now_ts(),
            usage: None,
        }
    }

    fn sample_session(dir: &tempfile::TempDir) -> Session {
        Session::create(dir.path(), "openai", "gpt-test").unwrap()
    }

    fn text_of(message: &Message) -> String {
        match &message.content[0] {
            Content::Text(text) => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    /// sessions 目录里属于该 id 的备份文件名（时间戳命名，排序后返回）。
    fn backups_of(dir: &tempfile::TempDir, id: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path().join(SESSIONS_SUBDIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&format!("{id}.")) && n.ends_with(BACKUP_SUFFIX))
            .collect();
        names.sort();
        names
    }

    fn dir_files(dir: &tempfile::TempDir) -> Vec<String> {
        fs::read_dir(dir.path().join(SESSIONS_SUBDIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn append_flushes_and_resumes_back() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        session
            .append(Message::assistant(
                vec![Content::Text("hello".into())],
                Some(Usage::default()),
            ))
            .unwrap();
        let raw = fs::read_to_string(&session.path).unwrap();
        assert_eq!(raw.lines().count(), 3);

        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.header, session.header);
        assert_eq!(resumed.messages, session.messages);
        assert_eq!(resumed.path, session.path);
        assert_eq!(resumed.header.provider, "openai");
        assert_eq!(text_of(&resumed.messages[0]), "hi");
    }

    #[test]
    fn list_reads_only_header_lines() {
        let (dir, _guard) = temp_data_dir();
        let a = sample_session(&dir);
        let b = sample_session(&dir);
        fs::write(a.path.with_extension("other.txt"), b"ignore me\n").unwrap();
        let ids: Vec<String> = Session::list().unwrap().into_iter().map(|h| h.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a.header.id));
        assert!(ids.contains(&b.header.id));
    }

    #[test]
    fn open_or_resume_creates_then_resumes_last_and_by_id() {
        let (dir, _guard) = temp_data_dir();
        let created = Session::open_or_resume(None, dir.path(), "openai", "gpt-test").unwrap();
        let reopened =
            Session::open_or_resume(Some("last"), dir.path(), "openai", "gpt-test").unwrap();
        assert_eq!(reopened.header.id, created.header.id);
        let by_id =
            Session::open_or_resume(Some(&created.header.id), dir.path(), "x", "y").unwrap();
        assert_eq!(by_id.header.id, created.header.id);
        assert!(Session::open_or_resume(Some("last"), dir.path(), "openai", "gpt-test").is_ok());
    }

    #[test]
    fn atomic_rewrite_keeps_bak_and_replaces_content() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        session
            .append(Message::assistant(vec![Content::Text("two".into())], None))
            .unwrap();
        let old_raw = fs::read_to_string(&session.path).unwrap();

        let summary = msg(
            Role::User,
            &format!("{} kept tail", message::SUMMARY_PREFIX),
        );
        session.rewrite(vec![summary.clone()]).unwrap();

        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), 1, "{baks:?}");
        // 备份名可识别：`<id>.<epoch 秒>.bak.jsonl`（同一秒冲突时再带计数器）。
        let ts: i64 = baks[0][session.header.id.len() + 1..]
            .split('.')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(ts > 0, "{baks:?}");
        let bak = dir.path().join(SESSIONS_SUBDIR).join(&baks[0]);
        assert_eq!(fs::read_to_string(&bak).unwrap(), old_raw);

        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(text_of(&resumed.messages[0]), text_of(&summary));

        session.rewrite(vec![msg(Role::User, "again")]).unwrap();
        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), 2, "{baks:?}");
        // 备份与随机后缀临时文件都不出现在 list 里；rewrite 成功后无 tmp 残留。
        assert_eq!(Session::list().unwrap().len(), 1);
        let leftovers: Vec<String> = dir_files(&dir)
            .into_iter()
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn validate_session_id_accepts_uuid_like_and_rejects_paths() {
        assert!(validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_session_id("abc_DEF.-123").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "...",
            "/etc/passwd",
            "/abs/id.jsonl",
            "../victim",
            "a/b",
            "a\\b",
            "./x",
            "with space",
            "ünicode",
            "a\u{0}b",
            "$(cmd)",
        ] {
            assert!(validate_session_id(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn traversal_ids_cannot_read_or_delete_outside_sessions() {
        let (dir, _guard) = temp_data_dir();
        sample_session(&dir); // 建出 sessions 目录
        let victim = dir.path().join("victim.jsonl");
        let victim_header = SessionHeader {
            id: "victim".into(),
            created: 1,
            cwd: dir.path().to_path_buf(),
            provider: "p".into(),
            model: "m".into(),
        };
        fs::write(
            &victim,
            format!("{}\n", serde_json::to_string(&victim_header).unwrap()),
        )
        .unwrap();
        // 相对 parent、绝对路径、含分隔符的 id 全部在校验层被拒，不触碰 fs。
        for bad in [
            "../victim",
            "/../victim",
            dir.path().join("victim").to_str().unwrap(),
        ] {
            let err = Session::resume(bad).unwrap_err().to_string();
            assert!(err.contains("invalid session id"), "{err}");
            let err = Session::remove(bad).unwrap_err().to_string();
            assert!(err.contains("invalid session id"), "{err}");
            let err = Session::open_or_resume(Some(bad), dir.path(), "p", "m")
                .unwrap_err()
                .to_string();
            assert!(err.contains("invalid session id"), "{err}");
        }
        assert!(victim.exists(), "victim must survive rejected ids");
        assert_eq!(Session::list().unwrap().len(), 1);
    }

    #[test]
    fn symlinked_session_file_is_not_followed() {
        let (dir, _guard) = temp_data_dir();
        sample_session(&dir);
        let victim = dir.path().join("victim.jsonl");
        fs::write(&victim, b"{}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, dir.path().join(SESSIONS_SUBDIR).join("link.jsonl"))
            .unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(
            &victim,
            dir.path().join(SESSIONS_SUBDIR).join("link.jsonl"),
        )
        .unwrap();
        // list / last / resume 都跳过或拒绝 symlink 主文件，不读根目录外内容。
        assert_eq!(Session::list().unwrap().len(), 1);
        let err = Session::resume("link").unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(fs::read_to_string(&victim).unwrap().starts_with('{'));
    }

    #[test]
    fn list_skips_bad_sessions_and_keeps_valid_ones() {
        let (dir, _guard) = temp_data_dir();
        let a = sample_session(&dir);
        let sessions = dir.path().join(SESSIONS_SUBDIR);
        // 坏 header。
        fs::write(sessions.join("deadbeef.jsonl"), b"{ broken\n").unwrap();
        // header id 与文件名不一致。
        let liar = SessionHeader {
            id: "someone-else".into(),
            created: 5,
            cwd: dir.path().to_path_buf(),
            provider: "p".into(),
            model: "m".into(),
        };
        fs::write(
            sessions.join("liar.jsonl"),
            format!("{}\n", serde_json::to_string(&liar).unwrap()),
        )
        .unwrap();
        // 空文件（ended before line 1）。
        fs::write(sessions.join("empty.jsonl"), b"").unwrap();
        // 非法 stem 名。
        fs::write(sessions.join("bad name.jsonl"), b"{}\n").unwrap();

        let ids: Vec<String> = Session::list().unwrap().into_iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![a.header.id.clone()]);
        // 诊断带文件路径（list 把同类错误打到 stderr）。
        let err = load_header(&sessions.join("deadbeef.jsonl"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("deadbeef.jsonl"), "{err}");
        let err = load_header(&sessions.join("liar.jsonl"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("liar.jsonl") && err.contains("header id"),
            "{err}"
        );
    }

    #[test]
    fn last_skips_bad_files_and_diagnoses_when_nothing_valid() {
        let (dir, _guard) = temp_data_dir();
        let good = sample_session(&dir);
        let sessions = dir.path().join(SESSIONS_SUBDIR);
        // 比 good 更新但 header 坏：不得遮蔽有效会话。
        std::thread::sleep(std::time::Duration::from_millis(10));
        let bad = sessions.join("ffff0000.jsonl");
        fs::write(&bad, b"{ broken\n").unwrap();
        let resumed = Session::open_or_resume(Some("last"), dir.path(), "p", "m").unwrap();
        assert_eq!(resumed.header.id, good.header.id);

        Session::remove(&good.header.id).unwrap();
        let err = Session::open_or_resume(Some("last"), dir.path(), "p", "m")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no valid session"), "{err}");
        // 空目录才新建。
        fs::remove_file(&bad).unwrap();
        let fresh = Session::open_or_resume(Some("last"), dir.path(), "p", "m").unwrap();
        assert_eq!(fresh.messages.len(), 0);
    }

    #[test]
    fn salvage_rewrites_prefix_and_append_survives_next_resume() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        session
            .append(Message::assistant(vec![Content::Text("yo".into())], None))
            .unwrap();
        let good_raw = fs::read_to_string(&session.path).unwrap();
        fs::write(&session.path, format!("{good_raw}garbage\nhalf-broken\n")).unwrap();

        let mut resumed = Session::resume(&session.header.id).unwrap();
        // 坏尾部被物理修复：主文件只剩 header + 2 条合法消息。
        let raw = fs::read_to_string(&session.path).unwrap();
        assert_eq!(raw.lines().count(), 3, "{raw}");
        assert!(!raw.contains("garbage"), "{raw}");
        // 修复前的含坏行版本存为时间戳备份。
        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), 1, "{baks:?}");
        let bak_raw = fs::read_to_string(dir.path().join(SESSIONS_SUBDIR).join(&baks[0])).unwrap();
        assert!(bak_raw.contains("garbage"), "{bak_raw}");

        // resume→append→resume：新增消息不再被丢弃。
        resumed.append(msg(Role::User, "after")).unwrap();
        let again = Session::resume(&session.header.id).unwrap();
        assert_eq!(again.messages.len(), 3);
        assert_eq!(text_of(&again.messages[2]), "after");
    }

    #[test]
    fn backups_are_bounded_per_session() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        for i in 0..(MAX_BACKUPS + 3) {
            session
                .rewrite(vec![msg(Role::User, &format!("m{i}"))])
                .unwrap();
        }
        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), MAX_BACKUPS, "{baks:?}");
    }

    #[test]
    fn rename_failure_cleans_temp_file_and_keeps_error() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        // 用目录占住主文件路径：rename(文件 → 目录) 必然失败。
        fs::remove_file(&session.path).unwrap();
        fs::create_dir(&session.path).unwrap();
        let err = session
            .rewrite(vec![msg(Role::User, "new")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("move new session"), "{err}");
        let leftovers: Vec<String> = dir_files(&dir)
            .into_iter()
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        fs::remove_dir(&session.path).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unix_dirs_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        session.rewrite(vec![msg(Role::User, "two")]).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join(SESSIONS_SUBDIR)), 0o700);
        assert_eq!(mode(&session.path), 0o600);
        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), 1, "{baks:?}");
        assert_eq!(
            mode(&dir.path().join(SESSIONS_SUBDIR).join(&baks[0])),
            0o600
        );
    }

    #[test]
    fn rewrite_survives_backup_copy_failure_without_losing_main() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        // 主文件不可读 → bak 复制失败（步骤在 rename 之前，非致命），主文件全程存在。
        fs::set_permissions(&session.path, fs::Permissions::from_mode(0o000)).unwrap();
        session.rewrite(vec![msg(Role::User, "new")]).unwrap();
        fs::set_permissions(&session.path, fs::Permissions::from_mode(0o644)).unwrap();
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(text_of(&resumed.messages[0]), "new");
    }

    #[test]
    fn broken_header_reports_line_number() {
        let (dir, _guard) = temp_data_dir();
        let session = sample_session(&dir);
        fs::write(&session.path, "{ broken header\n").unwrap();
        let err = Session::resume(&session.header.id).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn salvage_counts_truncation_and_backtrack() {
        let good = serde_json::to_string(&msg(Role::User, "hi")).unwrap();
        // 坏行截断：坏行及其后的行全部计入丢弃。
        let (messages, dropped) = salvage(&[good.clone(), "not json".into(), good.clone()]);
        assert_eq!(messages.len(), 1);
        assert_eq!(dropped, 2);
        // 尾部 tool_use 无结果（合法 JSON、违反不变量 2）：回退到合法前缀。
        let orphan = Message::assistant(
            vec![Content::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            None,
        );
        let (messages, dropped) = salvage(&[good.clone(), serde_json::to_string(&orphan).unwrap()]);
        assert_eq!(messages.len(), 1);
        assert_eq!(dropped, 1);
        // 全坏正文：只剩 header。
        let (messages, dropped) = salvage(&["nope".into(), "{".into()]);
        assert!(messages.is_empty());
        assert_eq!(dropped, 2);
    }

    #[test]
    fn resume_salvages_corrupt_tail() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        session
            .append(Message::assistant(vec![Content::Text("yo".into())], None))
            .unwrap();
        let good_raw = fs::read_to_string(&session.path).unwrap();

        // 尾部追加手写坏行：resume 成功且保留合法前缀。
        fs::write(&session.path, format!("{good_raw}garbage-not-json\n")).unwrap();
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(text_of(&resumed.messages[1]), "yo");

        // 正文全坏：按空会话打开。
        fs::write(
            &session.path,
            format!(
                "{}\nnope\n",
                serde_json::to_string(&session.header).unwrap()
            ),
        )
        .unwrap();
        assert!(Session::resume(&session.header.id)
            .unwrap()
            .messages
            .is_empty());
    }

    #[test]
    fn resume_backtracks_broken_invariants_and_remove_deletes() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "a")).unwrap();
        // 老版本（append 不校验时）写入的违反不变量 1 的尾部：手写落盘。
        let raw = fs::read_to_string(&session.path).unwrap();
        fs::write(
            &session.path,
            format!(
                "{raw}{}\n",
                serde_json::to_string(&msg(Role::User, "b")).unwrap()
            ),
        )
        .unwrap();
        // 违反不变量 1 的尾部回退掉，保留合法前缀。
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(text_of(&resumed.messages[0]), "a");
        Session::remove(&session.header.id).unwrap();
        assert!(!session.path.exists());
        assert!(Session::remove(&session.header.id).is_err());
    }

    #[test]
    fn append_enforces_message_contract_at_boundary() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "run")).unwrap();
        // finish_turn 中间态合法：尾部 assistant 的 tool_use 允许先落盘。
        session
            .append(Message::assistant(
                vec![Content::ToolUse {
                    id: "a".into(),
                    name: "shell".into(),
                    input: serde_json::json!({}),
                }],
                None,
            ))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&session.path).unwrap().lines().count(),
            3
        );

        // 不配对就继续追加 → 拒绝：不落盘、不入库。
        let err = format!(
            "{:#}",
            session
                .append(msg(Role::User, "changed my mind"))
                .unwrap_err()
        );
        assert!(err.contains("invariant 2"), "{err}");
        assert!(err.contains("invalid append at message 2"), "{err}");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(
            fs::read_to_string(&session.path).unwrap().lines().count(),
            3
        );

        // 精确配对的结果正常落盘。
        session
            .append(Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "a".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                ts: 0,
                usage: None,
            })
            .unwrap();
        let again = Session::resume(&session.header.id).unwrap();
        assert_eq!(again.messages, session.messages);

        // 连续同角色（不变量 1）也在 append 边界拒绝。
        let err = format!(
            "{:#}",
            session.append(msg(Role::User, "twice")).unwrap_err()
        );
        assert!(err.contains("invariant 1"), "{err}");
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn resume_ignores_legacy_name_field() {
        let (dir, _guard) = temp_data_dir();
        let session = sample_session(&dir);
        let mut legacy = serde_json::to_value(&session.header).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .insert("name".into(), serde_json::json!("legacy-name"));
        fs::write(
            &session.path,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.header, session.header);
    }
}
