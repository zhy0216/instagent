//! 会话：JSONL，不用 sqlite（第二版 §2.6）。
//!
//! 路径 `<data_dir>/sessions/<id>.jsonl`；data_dir 用 etcetera，默认
//! `~/.local/share/instagent`，可被 `INSTAGENT_DATA_DIR` 覆盖（测试用）。
//! 第 1 行 header，之后每行一条 Message。

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::message::Message;

// TODO(02)：填实现（追加即 flush、list 只读首行、原子重写产生 `<id>.<n>.bak.jsonl`）。

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

impl Session {
    /// `<data_dir>/sessions/`，data_dir 解析见模块注释。TODO(02)
    pub fn sessions_dir() -> crate::Result<PathBuf> {
        todo!("TODO(02)")
    }

    /// 新建会话（uuid v4，写 header 行）。TODO(02)
    pub fn create(_cwd: &Path, _provider: &str, _model: &str) -> crate::Result<Session> {
        todo!("TODO(02)")
    }

    /// 全量读回，并跑 `message::validate`。TODO(02)
    pub fn resume(_id: &str) -> crate::Result<Session> {
        todo!("TODO(02)")
    }

    /// `id` 为 None 新建；Some("last") 取最近；否则按 id。TODO(02)/`18` 接线
    pub fn open_or_resume(
        _id: Option<&str>,
        _cwd: &Path,
        _provider: &str,
        _model: &str,
    ) -> crate::Result<Session> {
        todo!("TODO(02)")
    }

    /// 追加一行 + 立即 flush。TODO(02)
    pub fn append(&mut self, _message: Message) -> crate::Result<()> {
        todo!("TODO(02)")
    }

    /// 只读每个文件的第一行。TODO(02)
    pub fn list() -> crate::Result<Vec<SessionHeader>> {
        todo!("TODO(02)")
    }

    /// 删除会话文件（`sessions rm`）。TODO(02)
    pub fn remove(_id: &str) -> crate::Result<()> {
        todo!("TODO(02)")
    }

    /// 压缩用原子重写（`16` 调用）：header + 新内容写临时文件 → rename 覆盖，
    /// 旧文件改名 `<id>.<n>.bak.jsonl` 留着排错。TODO(02)
    pub fn rewrite(&mut self, _messages: Vec<Message>) -> crate::Result<()> {
        todo!("TODO(02)")
    }
}
