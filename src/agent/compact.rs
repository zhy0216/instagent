//! 自动压缩（第二版 §2.7）。触发：每条 assistant 消息后
//! `usage.input >= threshold × context_limit`；或用户 `/compact`（`18` 接）；
//! 或 provider 报 ContextOverflow（强制压缩后重试一次）。不引 tokenizer。
//!
//! TODO(16)：填实现。摘要请求 = 历史格式化成文本 + [`COMPACTION_PROMPT`]
//! （prompt 文本抄 goose `context-management/prompts/compaction.md`，搬运注明
//! 出处；v1 直接输出 markdown）。防摘要器自身溢出：>2KB 的 ToolResult 正文先
//! 替换成 `[truncated N bytes]`。结果：历史替换为一条 `SUMMARY_PREFIX` 开头的
//! User 消息 + 保留末尾未回答的 user 消息；经 `02` 的原子重写落盘；
//! 发 `Event::Compacted`。

use tokio::sync::mpsc;

use crate::agent::event::Event;
use crate::agent::Agent;
use crate::message::Usage;
use crate::session::Session;

/// 摘要 prompt 正文。TODO(16)
pub const COMPACTION_PROMPT: &str = "";

/// 超过此字节数的 ToolResult 正文先截断再送摘要器。
pub const TOOL_RESULT_TRUNCATE_BYTES: usize = 2048;

/// 阈值判定（threshold 默认 0.8）。TODO(16)
pub fn should_compact(_usage: &Usage, _context_limit: u32, _threshold: f32) -> bool {
    todo!("TODO(16)")
}

/// 每轮开头的条件压缩；发生则返回 true 并发 Event::Compacted。TODO(16)
pub async fn maybe(
    _agent: &Agent,
    _session: &mut Session,
    _events: &mpsc::Sender<Event>,
) -> crate::Result<bool> {
    todo!("TODO(16)")
}

/// ContextOverflow / `/compact` 的强制压缩。TODO(16)
pub async fn force(
    _agent: &Agent,
    _session: &mut Session,
    _events: &mpsc::Sender<Event>,
) -> crate::Result<()> {
    todo!("TODO(16)")
}
