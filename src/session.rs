//! 会话：JSONL，不用 sqlite（第二版 §2.6）。
//!
//! 路径 `<data_dir>/sessions/<id>.jsonl`；data_dir 用 etcetera，默认
//! `~/.local/share/instagent`，可被 `INSTAGENT_DATA_DIR` 覆盖（测试用）。
//! 第 1 行 header，之后每行一条 Message。

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub id: String,
    /// epoch 秒。
    pub created: i64,
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub header: SessionHeader,
    pub messages: Vec<Message>,
    /// `<data_dir>/sessions/<id>.jsonl`
    pub path: PathBuf,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
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

    /// 新建会话（uuid v4，写 header 行）。
    pub fn create(cwd: &Path, provider: &str, model: &str) -> crate::Result<Session> {
        let dir = Self::sessions_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("create sessions dir {}", dir.display()))?;
        let header = SessionHeader {
            id: Uuid::new_v4().to_string(),
            created: now_ts(),
            cwd: cwd.to_path_buf(),
            provider: provider.to_string(),
            model: model.to_string(),
            name: None,
        };
        let path = dir.join(format!("{}.{}", header.id, JSONL_EXT));
        let mut file = File::create(&path)
            .with_context(|| format!("create session file {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        file.flush()?;
        Ok(Session {
            header,
            messages: Vec::new(),
            path,
        })
    }

    /// 全量读回，并跑 `message::validate`。坏行报错带行号（header 为第 1 行）。
    pub fn resume(id: &str) -> crate::Result<Session> {
        let path = Self::sessions_dir()?.join(format!("{id}.{JSONL_EXT}"));
        let file =
            File::open(&path).with_context(|| format!("open session file {}", path.display()))?;
        let mut lines = BufReader::new(file).lines();
        let header: SessionHeader = {
            let raw = next_line(&mut lines, &path, 1)?;
            parse_line(&raw, &path, 1)?
        };
        let mut messages = Vec::new();
        let mut num = 2usize;
        while let Some(raw) = opt_line(&mut lines, &path, num)? {
            if !raw.trim().is_empty() {
                messages.push(parse_line(&raw, &path, num)?);
            }
            num += 1;
        }
        if header.id != id {
            bail!(
                "session {}: header id {:?} != requested {id:?}",
                path.display(),
                header.id
            );
        }
        message::validate(&messages).with_context(|| format!("validate session {id}"))?;
        Ok(Session {
            header,
            messages,
            path,
        })
    }

    /// `id` 为 None 新建；Some("last") 取最近（按文件 mtime）；否则按 id。TODO(`18`) 接线
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
                let latest = session_files(&dir)?.into_iter().max_by_key(|path| {
                    fs::metadata(path)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH)
                });
                match latest {
                    Some(path) => {
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        Self::resume(&stem)
                    }
                    None => Self::create(cwd, provider, model),
                }
            }
            Some(id) => Self::resume(id),
        }
    }

    /// 追加一行 + 立即 flush。
    pub fn append(&mut self, message: Message) -> crate::Result<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open session file for append {}", self.path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&message)?)?;
        file.flush()?;
        self.messages.push(message);
        Ok(())
    }

    /// 只读每个文件的第一行（header），按 created 降序。
    pub fn list() -> crate::Result<Vec<SessionHeader>> {
        let dir = Self::sessions_dir()?;
        let mut headers = Vec::new();
        for path in session_files(&dir)? {
            let file = File::open(&path)
                .with_context(|| format!("open session file {}", path.display()))?;
            let mut lines = BufReader::new(file).lines();
            let raw = next_line(&mut lines, &path, 1)?;
            let header: SessionHeader = parse_line(&raw, &path, 1)
                .with_context(|| format!("read header of {}", path.display()))?;
            headers.push(header);
        }
        headers.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        Ok(headers)
    }

    /// 删除会话文件（`sessions rm`）。
    pub fn remove(id: &str) -> crate::Result<()> {
        let path = Self::sessions_dir()?.join(format!("{id}.{JSONL_EXT}"));
        fs::remove_file(&path).with_context(|| format!("remove session file {}", path.display()))
    }

    /// 压缩用原子重写（`16` 调用）：header + 新内容写临时文件，旧文件改名
    /// `<id>.<n>.bak.jsonl` 留着排错，再把临时文件 rename 到位。
    pub fn rewrite(&mut self, messages: Vec<Message>) -> crate::Result<()> {
        message::validate(&messages)
            .with_context(|| format!("validate rewritten session {}", self.header.id))?;
        let tmp_path = self.path.with_extension(format!("{JSONL_EXT}.tmp"));
        {
            let mut file = File::create(&tmp_path)
                .with_context(|| format!("create temp file {}", tmp_path.display()))?;
            writeln!(file, "{}", serde_json::to_string(&self.header)?)?;
            for message in &messages {
                writeln!(file, "{}", serde_json::to_string(message)?)?;
            }
            file.flush()?;
            file.sync_all()?;
        }
        let bak = backup_path(&self.path);
        fs::rename(&self.path, &bak).with_context(|| {
            format!(
                "move old session {} to {}",
                self.path.display(),
                bak.display()
            )
        })?;
        fs::rename(&tmp_path, &self.path).with_context(|| {
            format!(
                "move new session {} to {}",
                tmp_path.display(),
                self.path.display()
            )
        })?;
        self.messages = messages;
        Ok(())
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
        if name.ends_with(".bak.jsonl") || name.ends_with(".jsonl.tmp") {
            continue;
        }
        if path.is_file() && name.ends_with(&format!(".{JSONL_EXT}")) {
            files.push(path);
        }
    }
    Ok(files)
}

/// 第一个不占用 `<id>.<n>.bak.jsonl` 的 n（从 1 起）。
fn backup_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let mut n = 1usize;
    loop {
        let candidate = dir.join(format!("{id}.{n}.bak.{JSONL_EXT}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
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
            ts: now_ts(),
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

        let summary = msg(
            Role::User,
            &format!("{} kept tail", message::SUMMARY_PREFIX),
        );
        session.rewrite(vec![summary.clone()]).unwrap();

        let bak = dir
            .path()
            .join(SESSIONS_SUBDIR)
            .join(format!("{}.1.bak.jsonl", session.header.id));
        assert!(bak.exists(), "expected {bak:?}");
        let bak_raw = fs::read_to_string(&bak).unwrap();
        assert!(bak_raw.contains("one"));
        assert!(bak_raw.contains("two"));

        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(text_of(&resumed.messages[0]), text_of(&summary));

        session.rewrite(vec![msg(Role::User, "again")]).unwrap();
        let bak2 = dir
            .path()
            .join(SESSIONS_SUBDIR)
            .join(format!("{}.2.bak.jsonl", session.header.id));
        assert!(bak2.exists());
        // 备份文件不出现在 list 里
        assert_eq!(Session::list().unwrap().len(), 1);
    }

    #[test]
    fn bad_line_reports_line_number() {
        let (dir, _guard) = temp_data_dir();
        let session = sample_session(&dir);
        fs::write(
            &session.path,
            format!(
                "{}\nnot-json\n",
                serde_json::to_string(&session.header).unwrap()
            ),
        )
        .unwrap();
        let err = Session::resume(&session.header.id).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");

        fs::write(&session.path, "{ broken header\n").unwrap();
        let err = Session::resume(&session.header.id).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn resume_rejects_broken_invariants_and_remove_deletes() {
        let (dir, _guard) = temp_data_dir();
        let mut session = sample_session(&dir);
        session.append(msg(Role::User, "a")).unwrap();
        session.append(msg(Role::User, "b")).unwrap();
        let err = Session::resume(&session.header.id).unwrap_err();
        assert!(
            err.to_string().contains("invariant 1") || format!("{err:#}").contains("invariant 1"),
            "{err:#}"
        );
        Session::remove(&session.header.id).unwrap();
        assert!(!session.path.exists());
        assert!(Session::remove(&session.header.id).is_err());
    }
}
