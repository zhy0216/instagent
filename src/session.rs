//! 会话：JSONL，不用 sqlite（第二版 §2.6）。
//!
//! 路径 `<data_dir>/sessions/<id>.jsonl`；data_dir 用 etcetera，默认
//! `~/.local/share/instagent`，可被 `INSTAGENT_DATA_DIR` 覆盖（测试用）。
//! 第 1 行 header，之后每行一条 Message。
//!
//! 边界与持久化契约（plan S01–S04/R10、ADR 0003 D6 承诺的应用层纵深防御）：
//! - 任意外部 session id 先过 [`validate_session_id`]（安全文件名白名单），
//!   拒绝空串、`.` / `..`、绝对路径、路径分隔符与非安全字符；
//!   create / resume / open_or_resume / remove / list 全部复用，保证 CLI 传入
//!   的 id 不会读/删 sessions 根目录外的文件。sessions 目录内的 symlink 主
//!   文件在读取与追加路径都拒绝（不跟随）。
//! - Unix：sessions 目录 0700；会话主文件 / 临时文件 / 备份 0600（备份从创建
//!   起即私有，无权限继承窗口）。
//! - durability：append 至少 flush（进程崩溃不丢，主机掉电不保证）；create /
//!   rewrite / salvage 修复写回额外 `sync_all`，rename 后尽力同步父目录。
//! - 有界读取（S01）：header / 正文按字节逐行带预算读取（见
//!   `DEFAULT_LIMITS`），超行/总预算即拒绝打开，原文件字节保持原状，不借
//!   salvage 删除超预算内容；错误只带路径 / 行号 / 约束，不回显行原文
//!   （假密钥不进 stderr）。
//! - resume salvage（S02）：正文坏 UTF-8 / 坏 JSON / 违反不变量的尾部保留合法
//!   前缀并**物理写回**主 JSONL；有效最后一行缺换行同样规范化写回，保证之后
//!   的 append 不粘连；header 损坏仍明确报错、不 salvage。
//! - append / [`Session::append_batch`]（S03）：先预序列化 + 校验候选历史再
//!   写入；记录原文件长度，检测到写/flush 失败回退文件长度并保持内存旧状态；
//!   回退也失败时明确要求重新恢复、不报成功。只承诺已检测 IO 错误的回退，
//!   不宣称主机掉电时的事务原子性。
//! - 原子重写：header+消息写随机后缀私有临时文件，旧主文件先复制为带时间戳
//!   的 `<id>.<ts>.bak.jsonl` 备份（best-effort，每会话保留最近
//!   `MAX_BACKUPS` 份），再 rename 覆盖主文件；临时文件创建后任意
//!   写/flush/sync/rename 失败都清理临时文件。不变量：主文件任何时刻要么是
//!   旧内容要么是新内容，绝不缺失。
//! - 备份归属按精确命名 `<id>.<数字>[.<数字>].bak.jsonl` 解析（S04），会话
//!   `a` 的清理不会数到或删除会话 `a.b` 的备份。
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

use anyhow::anyhow;
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

/// 读取预算（plan §预算默认）。测试可注入小值避免制造百 MB fixture。
#[derive(Clone, Copy, Debug)]
struct ReadLimits {
    header_bytes: usize,
    line_bytes: usize,
    total_bytes: u64,
}

/// 默认预算：
/// - header 64 KiB：header JSON 只有几百字节，留百倍余量；
/// - 单消息行 96 MiB：最大图片 20 MiB 解码 → base64 ≈26.7 MiB，加 JSON
///   开销后仍有 3 倍以上余量（见 `default_limits_cover_largest_image_message`）；
/// - 总文件 256 MiB：兼容单会话 64 MiB 图片解码预算下的多图历史。
const DEFAULT_LIMITS: ReadLimits = ReadLimits {
    header_bytes: 64 * 1024,
    line_bytes: 96 * 1024 * 1024,
    total_bytes: 256 * 1024 * 1024,
};

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

    /// 全量读回（默认预算 `DEFAULT_LIMITS`）。id 先过
    /// [`validate_session_id`]。header（第 1 行）损坏明确报错（只带路径 / 行号
    /// / 约束，不回显原文）；正文按"合法前缀"恢复：坏 UTF-8 / 坏 JSON 行即
    /// 截断，再回退到最近一条满足 `message::validate` 的前缀，丢弃行数向
    /// stderr 警告；只剩 header 时按空会话打开。有丢弃或有效最后一行缺换行
    /// 时把合法内容**原子写回主 JSONL**（旧内容存时间戳备份）——否则之后的
    /// append 落在坏行之后（或与无换行尾行粘连），下次 resume 会再次丢弃
    /// 同一尾部，造成永久数据丢失（S2）。超行/总预算直接拒绝打开，原文件
    /// 字节保持原状。
    pub fn resume(id: &str) -> crate::Result<Session> {
        resume_with_limits(id, DEFAULT_LIMITS)
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

    /// 追加单条消息：签名保持不变，等价于单条 [`Session::append_batch`]。
    pub fn append(&mut self, message: Message) -> crate::Result<()> {
        self.append_batch(vec![message])
    }

    /// 成批追加消息（供一次性提交 assistant + tool results；S03）。
    ///
    /// 步骤：预序列化全部新消息（失败 → 零落盘、内存不变）→ 扩展候选历史并
    /// 经 `message::validate_for_append` 统一校验核心 → 记录原文件长度后逐行
    /// 追加 + flush。校验失败 → 零落盘；检测到的写 / flush 失败 → 文件回退到
    /// 原长度且内存保持旧状态；回退也失败 → 错误明确要求重新恢复（再 resume
    /// 走 salvage），不报成功。错误只带会话 id / 消息索引 / 约束，不回显消息
    /// 原文。只承诺已检测 IO 错误的回退，不宣称主机掉电时的事务原子性。
    pub fn append_batch(&mut self, messages: Vec<Message>) -> crate::Result<()> {
        let start = self.messages.len();
        let mut lines = Vec::with_capacity(messages.len());
        for (offset, message) in messages.iter().enumerate() {
            let line = serde_json::to_string(message).with_context(|| {
                format!(
                    "serialize message {} for session {}",
                    start + offset,
                    self.header.id
                )
            })?;
            lines.push(line);
        }
        if lines.is_empty() {
            return Ok(());
        }
        self.messages.extend(messages);
        let committed = message::validate_for_append(&self.messages)
            .with_context(|| {
                format!(
                    "invalid append at message {start} in session {}",
                    self.header.id
                )
            })
            .and_then(|()| append_lines(&self.path, &lines));
        if committed.is_err() {
            self.messages.truncate(start);
        }
        committed
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

/// 创建私有（unix 0600）新文件：header、临时文件、备份共用。
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
/// id、主文件不是 symlink、文件大小在总预算内、header 行可解析且
/// header.id 与文件名一致。错误信息自带文件路径，不回显行原文（S01）。
fn load_header(path: &Path) -> crate::Result<SessionHeader> {
    read_header(path, DEFAULT_LIMITS).map(|(header, _)| header)
}

/// 有界读取并校验 header，返回 header 行是否以换行结束（resume 需要）。
fn read_header(path: &Path, limits: ReadLimits) -> crate::Result<(SessionHeader, bool)> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("session file name not valid UTF-8: {}", path.display()))?;
    validate_session_id(id).with_context(|| format!("bad session file {}", path.display()))?;
    let file = open_session_file(path)?;
    let len = file
        .metadata()
        .with_context(|| format!("stat session file {}", path.display()))?
        .len();
    if len > limits.total_bytes {
        bail!(
            "session file {} is {} bytes, over the {}-byte total budget; refusing to open \
             (file left untouched)",
            path.display(),
            len,
            limits.total_bytes
        );
    }
    let mut reader = BufReader::new(file);
    let mut read_total = 0u64;
    let Some((mut bytes, terminated)) = read_line_bounded(
        &mut reader,
        limits.header_bytes,
        limits.total_bytes,
        &mut read_total,
        path,
        1,
    )?
    else {
        bail!("session file {} ended before line 1", path.display());
    };
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("header (line 1) of {} is not valid UTF-8", path.display()))?;
    let header: SessionHeader = serde_json::from_str(raw)
        .with_context(|| format!("invalid header JSON at line 1 of {}", path.display()))?;
    if header.id != id {
        bail!(
            "session {}: header id {:?} != file name {id:?}",
            path.display(),
            header.id
        );
    }
    Ok((header, terminated))
}

fn resume_with_limits(id: &str, limits: ReadLimits) -> crate::Result<Session> {
    let path = session_path(&Session::sessions_dir()?, id)?;
    let (header, header_terminated) = read_header(&path, limits)?;
    let file = open_session_file(&path)?;
    let mut reader = BufReader::new(file);
    let mut read_total = 0u64;
    // header 刚校验过；此处有界跳过以对齐正文读取位置。
    let header_terminated = match read_line_bounded(
        &mut reader,
        limits.header_bytes,
        limits.total_bytes,
        &mut read_total,
        &path,
        1,
    )? {
        Some((_, terminated)) => terminated,
        None => header_terminated,
    };
    let (messages, dropped, unterminated) =
        salvage_body(&mut reader, limits, &mut read_total, &path)?;
    if dropped > 0 {
        eprintln!(
            "warning: session {}: dropped {dropped} unrecoverable line(s), kept {} message(s)",
            path.display(),
            messages.len()
        );
        atomic_replace(&path, &header, &messages)
            .with_context(|| format!("repair salvaged session {}", path.display()))?;
    } else if !header_terminated || unterminated {
        eprintln!(
            "warning: session {}: normalized missing trailing newline before further appends",
            path.display()
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

/// 有界读取一行（到 `\n` 或 EOF），返回（行字节（不含换行），是否以换行结
/// 束）。行长度超 `line_budget` 或累计读取超 `total_budget` 即报错；错误只
/// 带路径 / 行号 / 预算，不回显内容（S01）。
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    line_budget: usize,
    total_budget: u64,
    read_total: &mut u64,
    path: &Path,
    num: usize,
) -> crate::Result<Option<(Vec<u8>, bool)>> {
    let mut buf = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .with_context(|| format!("read line {num} of {}", path.display()))?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some((buf, false))
            });
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let piece_len = newline.unwrap_or(chunk.len());
        buf.extend_from_slice(&chunk[..piece_len]);
        let consumed = piece_len + usize::from(newline.is_some());
        reader.consume(consumed);
        *read_total += consumed as u64;
        if buf.len() > line_budget {
            bail!(
                "line {num} of {} exceeds the {}-byte line budget; refusing to open \
                 (file left untouched)",
                path.display(),
                line_budget
            );
        }
        if *read_total > total_budget {
            bail!(
                "session file {} exceeds the {}-byte total read budget at line {num}; \
                 refusing to open (file left untouched)",
                path.display(),
                total_budget
            );
        }
        if newline.is_some() {
            return Ok(Some((buf, true)));
        }
    }
}

/// 坏行之后剩余物理行数（只扫描计数不缓冲；累计仍受总预算约束）。
fn count_tail_lines<R: BufRead>(
    reader: &mut R,
    total_budget: u64,
    read_total: &mut u64,
    path: &Path,
) -> crate::Result<usize> {
    let mut newlines = 0usize;
    let mut tail_bytes = 0usize;
    loop {
        let chunk = reader
            .fill_buf()
            .with_context(|| format!("read tail of {}", path.display()))?;
        if chunk.is_empty() {
            break;
        }
        for &byte in chunk {
            if byte == b'\n' {
                newlines += 1;
                tail_bytes = 0;
            } else {
                tail_bytes += 1;
            }
        }
        let consumed = chunk.len();
        reader.consume(consumed);
        *read_total += consumed as u64;
        if *read_total > total_budget {
            bail!(
                "session file {} exceeds the {}-byte total read budget; refusing to open \
                 (file left untouched)",
                path.display(),
                total_budget
            );
        }
    }
    Ok(newlines + usize::from(tail_bytes > 0))
}

/// 从正文（header 之后）流式恢复最长合法前缀：坏 UTF-8 / 坏 JSON 行即截断
/// （S2：不再像旧 `BufRead::lines` 那样在 salvage 前整体失败），再回退到满足
/// `message::validate` 的前缀（如尾部 tool_use 缺结果）。返回（消息，丢弃行
/// 数，有效最后一行是否缺换行）。
/// ponytail: 回退是 O(n²) 全量 validate；会话规模几百条，够用。
fn salvage_body<R: BufRead>(
    reader: &mut R,
    limits: ReadLimits,
    read_total: &mut u64,
    path: &Path,
) -> crate::Result<(Vec<Message>, usize, bool)> {
    let mut messages = Vec::new();
    let mut dropped = 0usize;
    let mut unterminated = false;
    let mut num = 2usize;
    while let Some((mut bytes, terminated)) = read_line_bounded(
        reader,
        limits.line_bytes,
        limits.total_bytes,
        read_total,
        path,
        num,
    )? {
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let blank = bytes.iter().all(u8::is_ascii_whitespace);
        if !terminated {
            // 只有文件最后一行可能缺换行；空白尾行对后续 append 无粘连风险。
            unterminated = !blank;
        }
        if !blank {
            let parsed = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|raw| serde_json::from_str::<Message>(raw).ok());
            match parsed {
                Some(message) => messages.push(message),
                None => {
                    dropped = 1 + count_tail_lines(reader, limits.total_bytes, read_total, path)?;
                    break;
                }
            }
        }
        num += 1;
    }
    while !messages.is_empty() && message::validate(&messages).is_err() {
        messages.pop();
        dropped += 1;
    }
    Ok((messages, dropped, unterminated))
}

/// 追加目标：生产为主文件 [`File`]；测试注入短写 / 写失败与回退失败（T2）。
trait AppendTarget {
    fn write_line(&mut self, line: &str) -> std::io::Result<()>;
    fn finish(&mut self) -> std::io::Result<()>;
    /// 检测到写失败后把文件截回追加前长度。
    fn rollback(&mut self, old_len: u64) -> std::io::Result<()>;
}

impl AppendTarget for File {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.write_all(line.as_bytes())?;
        self.write_all(b"\n")
    }
    fn finish(&mut self) -> std::io::Result<()> {
        self.flush()
    }
    fn rollback(&mut self, old_len: u64) -> std::io::Result<()> {
        self.set_len(old_len)
    }
}

/// 写入全部预序列化行 + flush；失败回退到 `old_len` 并保持调用方内存旧状态。
/// 回退也失败时明确要求重新恢复，绝不报成功（S03）。
fn write_batch_to<T: AppendTarget>(
    target: &mut T,
    old_len: u64,
    lines: &[String],
) -> crate::Result<()> {
    let written = lines
        .iter()
        .try_for_each(|line| target.write_line(line))
        .and_then(|()| target.finish());
    if let Err(err) = written {
        if let Err(rollback) = target.rollback(old_len) {
            bail!(
                "append failed ({err}) and rolling the file back to its previous {old_len} \
                 bytes also failed ({rollback}); the file may hold a partial line — resume \
                 the session to salvage it before appending again"
            );
        }
        return Err(err.into());
    }
    Ok(())
}

/// 把预序列化行追加到主文件：拒绝 symlink（与读取路径同一判定）；记录原长
/// 度；检测到的写 / flush 失败回退文件长度。
fn append_lines(path: &Path, lines: &[String]) -> crate::Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("open session file for append {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "refusing to append to symlinked session file {}",
            path.display()
        );
    }
    let old_len = meta.len();
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("open session file for append {}", path.display()))?;
    write_batch_to(&mut file, old_len, lines)
        .with_context(|| format!("append to session file {}", path.display()))
}

/// 原子重写主 JSONL（rewrite 与 salvage 修复共用）：header+消息写随机后缀
/// 私有临时文件（flush + sync_all），临时文件创建后任意写 / flush / sync 失败
/// 都清理临时文件；旧主文件复制为带时间戳备份（从创建起 0600，best-effort，
/// 失败只警告）；rename 覆盖主文件，失败清理临时文件并报错；成功后尽力同步
/// 父目录（unix，best-effort）。
fn atomic_replace(path: &Path, header: &SessionHeader, messages: &[Message]) -> crate::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let tmp_path = dir.join(format!("{stem}.{}{TMP_SUFFIX}", Uuid::new_v4()));
    if let Err(err) = write_replacement(&tmp_path, header, messages) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| format!("write temp session file {}", tmp_path.display()));
    }
    let bak = backup_path(path);
    match copy_private_file(path, &bak) {
        Ok(()) => prune_backups(dir, stem),
        Err(err) => tracing::warn!(
            "session {stem}: backup copy to {} failed: {err:#}",
            bak.display()
        ),
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

/// 写替换内容到临时文件：创建（私有 0600）+ 写行 + flush + sync_all。
fn write_replacement(
    tmp_path: &Path,
    header: &SessionHeader,
    messages: &[Message],
) -> crate::Result<()> {
    let mut file = create_private(tmp_path)
        .with_context(|| format!("create temp file {}", tmp_path.display()))?;
    writeln!(file, "{}", serde_json::to_string(header)?)?;
    for message in messages {
        writeln!(file, "{}", serde_json::to_string(message)?)?;
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// 复制主文件为备份：目标从创建起即 0600（私有），无权限继承窗口（T3）。
fn copy_private_file(src: &Path, dst: &Path) -> crate::Result<()> {
    let mut input =
        File::open(src).with_context(|| format!("open {} for backup", src.display()))?;
    let mut output =
        create_private(dst).with_context(|| format!("create backup {}", dst.display()))?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    output.flush()?;
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

/// 精确备份命名归属（S04）：`<id>.<数字>.bak.jsonl` 或同秒冲突的
/// `<id>.<数字>.<数字>.bak.jsonl`。前缀匹配会把合法含点的会话 `a.b` 的备份
/// 误算进 `a`，这里必须整名解析。
fn is_backup_name(name: &str, id: &str) -> bool {
    let Some(rest) = name.strip_suffix(BACKUP_SUFFIX) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(id) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    let (ts, counter) = match rest.split_once('.') {
        None => (rest, None),
        Some((ts, n)) => (ts, Some(n)),
    };
    if ts.is_empty() || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    counter.is_none_or(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// 每会话保留最近 [`MAX_BACKUPS`] 份备份，更老的删除（best-effort，R10）。
/// ponytail: read_dir + mtime 排序；备份数量上限个位数，够用。
fn prune_backups(dir: &Path, id: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| is_backup_name(n, id))
        })
        .collect();
    if backups.len() <= MAX_BACKUPS {
        return;
    }
    backups.sort_by_cached_key(|path| {
        (
            fs::metadata(path)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH),
            path.clone(),
        )
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

    /// sessions 目录里属于该 id 的备份文件名（精确命名判定，排序后返回）。
    fn backups_of(dir: &tempfile::TempDir, id: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path().join(SESSIONS_SUBDIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| is_backup_name(n, id))
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

    fn limits(header: usize, line: usize, total: u64) -> ReadLimits {
        ReadLimits {
            header_bytes: header,
            line_bytes: line,
            total_bytes: total,
        }
    }

    fn salvage_from(raw: &[u8]) -> (Vec<Message>, usize, bool) {
        let mut reader = BufReader::new(std::io::Cursor::new(raw.to_vec()));
        let mut read_total = 0u64;
        salvage_body(
            &mut reader,
            DEFAULT_LIMITS,
            &mut read_total,
            Path::new("test.jsonl"),
        )
        .unwrap()
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
    fn backup_copy_failure_is_bounded_and_keeps_source() {
        // 私有 IO 注入：dst 是目录 / src 缺失 → 复制失败；不用真实权限或
        // /dev/full。备份步骤在 atomic_replace 里是 warn-only（best-effort），
        // 此处断言失败有界（只带路径）且源文件不受影响。
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        let before = fs::read(&session.path).unwrap();
        let sessions = dir.path().join(SESSIONS_SUBDIR);

        // dst 是已存在目录 → 创建备份失败。
        let dst_dir = sessions.join("bak-block");
        fs::create_dir(&dst_dir).unwrap();
        let err = format!(
            "{:#}",
            copy_private_file(&session.path, &dst_dir).unwrap_err()
        );
        assert!(err.contains("backup") || err.contains("copy"), "{err}");
        assert!(err.len() < 1000, "backup error must stay bounded: {err}");
        assert!(!err.contains("one\n"), "{err}");
        assert_eq!(fs::read(&session.path).unwrap(), before);

        // src 缺失 → 同样有界失败，不产生目标文件。
        let missing = sessions.join("missing-src.jsonl");
        let dst = sessions.join("dst.123.bak.jsonl");
        let err = format!("{:#}", copy_private_file(&missing, &dst).unwrap_err());
        assert!(err.contains("backup") || err.contains("open"), "{err}");
        assert!(err.len() < 1000, "backup error must stay bounded: {err}");
        assert!(!dst.exists());
        assert_eq!(fs::read(&session.path).unwrap(), before);
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
        let (messages, dropped, _) = salvage_from(format!("{good}\nnot json\n{good}\n").as_bytes());
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
        let (messages, dropped, _) = salvage_from(
            format!("{good}\n{}\n", serde_json::to_string(&orphan).unwrap()).as_bytes(),
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(dropped, 1);
        // 全坏正文：只剩 header。
        let (messages, dropped, _) = salvage_from(b"nope\n{\n");
        assert!(messages.is_empty());
        assert_eq!(dropped, 2);
        // 坏 UTF-8 行不再让读取整体失败，同样按坏行截断（S2）。
        let (messages, dropped, _) = salvage_from(&[b'{', 0xff, b'\n']);
        assert!(messages.is_empty());
        assert_eq!(dropped, 1);
        // 有效最后一行缺换行：消息保留并报告需要规范化。
        let (messages, dropped, unterminated) = salvage_from(good.as_bytes());
        assert_eq!(messages.len(), 1);
        assert_eq!(dropped, 0);
        assert!(unterminated);
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

    // ---- todo 02：T1 有界读取与可再次恢复 ----

    #[test]
    fn header_errors_carry_path_line_constraint_but_no_raw() {
        let (dir, _guard) = temp_data_dir();
        let session = sample_session(&dir);
        // 假密钥标记 + 未闭合 JSON：错误只带路径/行号/约束，不回显原文（S01）。
        let fake_secret = "AKIAFAKESECRET123456";
        fs::write(
            &session.path,
            format!("{{\"id\":\"x\",\"token\":\"{fake_secret}\"\n"),
        )
        .unwrap();
        let err = format!("{:#}", Session::resume(&session.header.id).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("session"), "{err}");
        assert!(!err.contains(fake_secret), "{err}");
        assert!(!err.contains("token"), "{err}");

        // 坏 UTF-8 header 同样不回显。
        fs::write(&session.path, [0xffu8, 0xfe, b'\n']).unwrap();
        let err = Session::resume(&session.header.id).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn corrupt_utf8_tail_repairs_then_append_and_resume_keeps_new_message() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        let good_raw = fs::read(&session.path).unwrap();
        // 坏 UTF-8 尾行：旧实现在读取层直接报错，现在纳入 salvage（S02）。
        let mut corrupt = good_raw.clone();
        corrupt.extend_from_slice(b"\xff\xfe broken\n");
        fs::write(&session.path, &corrupt).unwrap();

        let mut resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(text_of(&resumed.messages[0]), "hi");
        // 物理修复 + 旧内容备份。
        let raw = fs::read(&session.path).unwrap();
        assert!(!raw.contains(&0xff), "repaired file must be clean UTF-8");
        assert_eq!(raw, good_raw);
        let baks = backups_of(&dir, &session.header.id);
        assert_eq!(baks.len(), 1, "{baks:?}");
        assert_eq!(
            fs::read(dir.path().join(SESSIONS_SUBDIR).join(&baks[0])).unwrap(),
            corrupt
        );

        // 恢复 → 追加合法 assistant → 再次恢复：新消息不丢。
        resumed
            .append(Message::assistant(vec![Content::Text("yo".into())], None))
            .unwrap();
        let again = Session::resume(&session.header.id).unwrap();
        assert_eq!(again.messages.len(), 2);
        assert_eq!(text_of(&again.messages[1]), "yo");
    }

    #[test]
    fn unterminated_header_or_message_is_normalized_before_append() {
        let (dir, _guard) = temp_data_dir();
        let session = sample_session(&dir);
        // header-only 文件缺尾换行：append 不能与 header 粘连。
        fs::write(
            &session.path,
            serde_json::to_string(&session.header).unwrap(),
        )
        .unwrap();
        let mut resumed = Session::resume(&session.header.id).unwrap();
        assert!(resumed.messages.is_empty());
        assert!(fs::read_to_string(&session.path).unwrap().ends_with('\n'));
        resumed.append(msg(Role::User, "first")).unwrap();
        let again = Session::resume(&session.header.id).unwrap();
        assert_eq!(again.messages.len(), 1);
        assert_eq!(text_of(&again.messages[0]), "first");

        // 有效最后一行缺尾换行：规范化后再追加，消息不粘连。
        let mut session2 = sample_session(&dir);
        session2.append(msg(Role::User, "one")).unwrap();
        let raw = fs::read_to_string(&session2.path).unwrap();
        fs::write(&session2.path, raw.trim_end_matches('\n')).unwrap();
        let mut resumed2 = Session::resume(&session2.header.id).unwrap();
        assert_eq!(resumed2.messages.len(), 1);
        assert!(fs::read_to_string(&session2.path).unwrap().ends_with('\n'));
        resumed2
            .append(Message::assistant(vec![Content::Text("two".into())], None))
            .unwrap();
        let again2 = Session::resume(&session2.header.id).unwrap();
        assert_eq!(again2.messages.len(), 2);
        assert_eq!(text_of(&again2.messages[0]), "one");
        assert_eq!(text_of(&again2.messages[1]), "two");
    }

    #[test]
    fn budget_refusal_keeps_file_bytes_untouched() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        session
            .append(Message::assistant(vec![Content::Text("yo".into())], None))
            .unwrap();
        let before = fs::read(&session.path).unwrap();

        // 总预算超限：拒绝打开，原文件字节保持原状，不借 salvage 删内容。
        let err = resume_with_limits(&session.header.id, limits(64 * 1024, 96 * 1024 * 1024, 64))
            .unwrap_err()
            .to_string();
        assert!(err.contains("total budget"), "{err}");
        assert_eq!(fs::read(&session.path).unwrap(), before);

        // header 行预算超限。
        let err = resume_with_limits(
            &session.header.id,
            limits(8, 96 * 1024 * 1024, 256 * 1024 * 1024),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("budget"), "{err}");
        assert_eq!(fs::read(&session.path).unwrap(), before);

        // 正文行预算超限。
        let err = resume_with_limits(&session.header.id, limits(64 * 1024, 4, 256 * 1024 * 1024))
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("budget"), "{err}");
        assert_eq!(fs::read(&session.path).unwrap(), before);

        // 默认预算下同一文件仍可正常打开。
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(fs::read(&session.path).unwrap(), before);
    }

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn image_session_resumes_intact() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "look")).unwrap();
        session
            .append(Message::assistant(vec![Content::Text("see".into())], None))
            .unwrap();
        session
            .append(Message {
                role: Role::User,
                content: vec![Content::Image(crate::tools::ImageData {
                    data: PNG_B64.into(),
                    media_type: "image/png".into(),
                })],
                ts: crate::message::now_ts(),
                usage: None,
            })
            .unwrap();
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages, session.messages);
    }

    #[test]
    fn default_limits_cover_largest_image_message() {
        // 单消息预算须容纳 20 MiB 图片的 base64（≈26.7 MiB）加 JSON 开销；
        // header / 总预算为 plan §预算默认 值。
        let max_image_b64 = (20 * 1024 * 1024usize).div_ceil(3) * 4;
        assert!(
            DEFAULT_LIMITS.line_bytes >= max_image_b64 + 1024 * 1024,
            "line budget must cover the largest allowed image message"
        );
        assert_eq!(DEFAULT_LIMITS.header_bytes, 64 * 1024);
        assert_eq!(DEFAULT_LIMITS.total_bytes, 256 * 1024 * 1024);
    }

    // ---- todo 02：T2 成批追加与失败清理 ----

    fn tool_use(id: &str) -> Content {
        Content::ToolUse {
            id: id.into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "ls"}),
        }
    }

    fn tool_result(id: &str) -> Content {
        Content::ToolResult {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }
    }

    #[test]
    fn append_batch_commits_assistant_and_results() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "run")).unwrap();
        session
            .append_batch(vec![
                Message::assistant(vec![tool_use("a"), tool_use("b")], None),
                Message {
                    role: Role::User,
                    content: vec![tool_result("a"), tool_result("b")],
                    ts: 0,
                    usage: None,
                },
            ])
            .unwrap();
        assert_eq!(session.messages.len(), 3);
        assert_eq!(
            fs::read_to_string(&session.path).unwrap().lines().count(),
            4
        );
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages, session.messages);
    }

    #[test]
    fn append_batch_invalid_writes_nothing_and_keeps_memory() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "run")).unwrap();
        let before = fs::read(&session.path).unwrap();

        // 连续 assistant 违反不变量 1：零落盘、内存不变，错误带索引不带原文。
        let err = format!(
            "{:#}",
            session
                .append_batch(vec![
                    Message::assistant(vec![Content::Text("x".into())], None),
                    Message::assistant(vec![Content::Text("y".into())], None),
                ])
                .unwrap_err()
        );
        assert!(err.contains("invariant 1"), "{err}");
        assert!(err.contains("invalid append at message 1"), "{err}");
        assert!(!err.contains("\"x\""), "{err}");
        assert_eq!(fs::read(&session.path).unwrap(), before);
        assert_eq!(session.messages.len(), 1);

        // orphan 结果同样拒绝。
        assert!(session
            .append_batch(vec![Message {
                role: Role::User,
                content: vec![tool_result("ghost")],
                ts: 0,
                usage: None,
            }])
            .is_err());
        assert_eq!(fs::read(&session.path).unwrap(), before);
        assert_eq!(session.messages.len(), 1);

        // 拒绝后仍能正常追加。
        session
            .append(Message::assistant(vec![Content::Text("ok".into())], None))
            .unwrap();
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn append_batch_empty_is_noop() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        let before = fs::read(&session.path).unwrap();
        session.append_batch(Vec::new()).unwrap();
        assert_eq!(fs::read(&session.path).unwrap(), before);
        assert!(session.messages.is_empty());
    }

    /// 短写 / 写失败注入：到 `fail_after` 字节后返回错误；`rollback_err`
    /// 控制回退是否也失败（T2 验收：私有 IO 注入，不动真实磁盘）。
    struct FakeSink {
        fail_after: usize,
        rollback_err: Option<std::io::ErrorKind>,
        written: Vec<u8>,
        rolled_back_to: Option<u64>,
    }

    impl AppendTarget for FakeSink {
        fn write_line(&mut self, line: &str) -> std::io::Result<()> {
            for &byte in line.as_bytes().iter().chain(std::iter::once(&b'\n')) {
                if self.written.len() >= self.fail_after {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                }
                self.written.push(byte);
            }
            Ok(())
        }
        fn finish(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn rollback(&mut self, old_len: u64) -> std::io::Result<()> {
            self.rolled_back_to = Some(old_len);
            match self.rollback_err {
                Some(kind) => Err(std::io::Error::from(kind)),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn injected_write_failure_rolls_back_to_old_length() {
        let mut sink = FakeSink {
            fail_after: 4,
            rollback_err: None,
            written: Vec::new(),
            rolled_back_to: None,
        };
        let lines = vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()];
        assert!(write_batch_to(&mut sink, 42, &lines).is_err());
        assert_eq!(sink.rolled_back_to, Some(42), "must truncate to old length");

        // 回退也失败：错误明确要求重新恢复，不报成功。
        let mut sink = FakeSink {
            fail_after: 4,
            rollback_err: Some(std::io::ErrorKind::PermissionDenied),
            written: Vec::new(),
            rolled_back_to: None,
        };
        let err = format!("{:#}", write_batch_to(&mut sink, 42, &lines).unwrap_err());
        assert!(err.contains("rolling the file back"), "{err}");
        assert!(err.contains("salvage"), "{err}");
        let lower = err.to_lowercase();
        assert!(lower.contains("write zero"), "{err}");
        assert!(lower.contains("permission denied"), "{err}");
    }

    #[test]
    fn append_rejects_symlinked_main_file() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        let victim = dir.path().join("victim.jsonl");
        fs::write(&victim, b"sentinel\n").unwrap();
        fs::remove_file(&session.path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &session.path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&victim, &session.path).unwrap();
        let err = session
            .append(Message::assistant(vec![Content::Text("x".into())], None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        // 目标文件不被写，内存保持旧状态。
        assert_eq!(fs::read_to_string(&victim).unwrap(), "sentinel\n");
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn append_open_failure_keeps_memory_and_leaves_disk_alone() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "hi")).unwrap();
        // 主文件路径变成目录 → 追加打开失败：内存回退，无写入发生。
        fs::remove_file(&session.path).unwrap();
        fs::create_dir(&session.path).unwrap();
        assert!(session
            .append_batch(vec![Message::assistant(
                vec![Content::Text("x".into())],
                None
            )])
            .is_err());
        assert_eq!(session.messages.len(), 1);
        fs::remove_dir(&session.path).unwrap();
    }

    // ---- todo 02：T3 临时文件与备份归属 ----

    #[test]
    fn replacement_write_failure_leaves_no_tmp_and_keeps_main() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "one")).unwrap();
        let before = fs::read(&session.path).unwrap();
        // write_replacement 创建失败（目标是目录）：错误可诊断，无残留。
        let sessions = dir.path().join(SESSIONS_SUBDIR);
        let tmp_dir = sessions.join(format!("x.{}", Uuid::new_v4()));
        fs::create_dir(&tmp_dir).unwrap();
        assert!(write_replacement(&tmp_dir, &session.header, &session.messages).is_err());
        fs::remove_dir(&tmp_dir).unwrap();
        // 主文件不受影响。
        assert_eq!(fs::read(&session.path).unwrap(), before);
        let leftovers: Vec<String> = dir_files(&dir)
            .into_iter()
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn is_backup_name_is_exact_per_session() {
        assert!(is_backup_name("a.123.bak.jsonl", "a"));
        assert!(is_backup_name("a.123.1.bak.jsonl", "a"));
        assert!(is_backup_name("a.b.123.bak.jsonl", "a.b"));
        // 会话 `a` 不得认领 `a.b` 的备份，也不得认领主文件或其它会话。
        assert!(!is_backup_name("a.b.123.bak.jsonl", "a"));
        assert!(!is_backup_name("a.jsonl", "a"));
        assert!(!is_backup_name("aa.123.bak.jsonl", "a"));
        assert!(!is_backup_name("a.x.bak.jsonl", "a"));
        assert!(!is_backup_name("a.123.x.bak.jsonl", "a"));
        assert!(!is_backup_name("a..bak.jsonl", "a"));
        assert!(!is_backup_name("b.123.bak.jsonl", "a"));
    }

    #[test]
    fn backup_ownership_is_independent_for_dotted_ids() {
        let (dir, _guard) = temp_data_dir();
        sample_session(&dir); // 建出 sessions 目录
        let sessions = dir.path().join(SESSIONS_SUBDIR);
        let hand = |id: &str| {
            let header = SessionHeader {
                id: id.into(),
                created: 1,
                cwd: dir.path().to_path_buf(),
                provider: "p".into(),
                model: "m".into(),
            };
            let path = sessions.join(format!("{id}.{JSONL_EXT}"));
            fs::write(
                &path,
                format!("{}\n", serde_json::to_string(&header).unwrap()),
            )
            .unwrap();
            Session {
                header,
                messages: Vec::new(),
                path,
            }
        };
        let mut a = hand("a");
        let mut ab = hand("a.b");
        // 预置两批同形备份：旧实现按前缀匹配会让 `a` 清理 `a.b` 的份数。
        for ts in 100..105 {
            fs::write(sessions.join(format!("a.{ts}.bak.jsonl")), b"old-a\n").unwrap();
            fs::write(sessions.join(format!("a.b.{ts}.bak.jsonl")), b"old-ab\n").unwrap();
        }
        for i in 0..3 {
            ab.rewrite(vec![msg(Role::User, &format!("ab{i}"))])
                .unwrap();
            a.rewrite(vec![msg(Role::User, &format!("a{i}"))]).unwrap();
        }
        // 各自计数独立：`a` 预置 5 + 新 3 → 修剪到 5；`a.b` 同样 5，互不侵犯。
        assert_eq!(backups_of(&dir, "a").len(), MAX_BACKUPS);
        assert_eq!(backups_of(&dir, "a.b").len(), MAX_BACKUPS);
        // 两会话最新内容均可恢复。
        let resumed_a = Session::resume("a").unwrap();
        assert_eq!(text_of(resumed_a.messages.last().unwrap()), "a2");
        let resumed_ab = Session::resume("a.b").unwrap();
        assert_eq!(text_of(resumed_ab.messages.last().unwrap()), "ab2");
    }

    #[test]
    fn same_second_backups_are_distinct_and_bounded() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        // 同一秒内多次备份退化为 `<id>.<ts>.<n>.bak.jsonl`，归属解析仍精确。
        for i in 0..(MAX_BACKUPS + 2) {
            session
                .rewrite(vec![msg(Role::User, &format!("v{i}"))])
                .unwrap();
        }
        let names = backups_of(&dir, &session.header.id);
        assert_eq!(names.len(), MAX_BACKUPS, "{names:?}");
        for name in &names {
            assert!(is_backup_name(name, &session.header.id), "{name}");
        }
    }
}
