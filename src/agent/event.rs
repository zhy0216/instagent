//! loop → UI 的单向事件（第二版 §2.5）。审批不走事件（需要答案，走
//! `agent::approval::Confirm` 回调）。

use serde_json::Value;

use crate::message::Usage;

#[derive(Debug, Clone)]
pub enum Event {
    TextDelta(String),
    ToolStart {
        id: String,
        name: String,
        input: Value,
    },
    ToolDone {
        id: String,
        preview: String,
        is_error: bool,
        elapsed_ms: u64,
    },
    Usage(Usage),
    Compacted {
        before_tokens: u32,
        after_tokens: u32,
    },
    Error(String),
}
