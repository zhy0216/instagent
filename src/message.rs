//! 消息模型（第二版 §2.2）。
//!
//! 会话不变量（构造时保证，`validate` 兜底，第二版 §2.2）：
//!
//! 1. user / assistant 严格交替；
//! 2. assistant 的每个 `ToolUse` 在紧接着的 user 消息里有同 id 的 `ToolResult`；
//! 3. 不发空 content；
//! 4. 被打断 / 出错的 tool call 补 `is_error = true` 的结果，
//!    文本 "interrupted by user"。

use serde::Deserialize;
use serde::Serialize;

use crate::tools::ToolCall;
use crate::tools::ToolOutput;

// TODO(02)：填构造器与 `validate`；类型布局已由 00 锁定。

/// 无 system 角色，system 是请求字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 无 Image / Thinking（v1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// 只有 assistant 消息有 usage；压缩触发直接用上一次响应的 `input`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    /// epoch 秒。
    pub ts: i64,
    pub usage: Option<Usage>,
}

/// 压缩摘要是一条以此开头的 User 消息（第二版 §2.2）。
pub const SUMMARY_PREFIX: &str = "# Conversation Summary";

impl Message {
    pub fn user_text(_text: String) -> Self {
        todo!("TODO(02)")
    }

    pub fn assistant(_content: Vec<Content>, _usage: Option<Usage>) -> Self {
        todo!("TODO(02)")
    }

    /// assistant 消息里的全部 ToolUse，供 loop 逐个审批执行（第二版 §2.5）。
    pub fn tool_uses(&self) -> Vec<ToolCall> {
        todo!("TODO(02)")
    }
}

impl Content {
    /// 不变量第 4 条：被打断 / 出错的 tool call 补错误结果。TODO(02)
    pub fn interrupted(_call: &ToolCall) -> Self {
        todo!("TODO(02)")
    }

    /// 工具执行结果 → ToolResult。TODO(02)
    pub fn tool_result(_call: &ToolCall, _out: ToolOutput) -> Self {
        todo!("TODO(02)")
    }
}

/// 检查四条会话不变量；`16` 在测试里每次 append 后都跑（第二版 §6 风险 2）。TODO(02)
pub fn validate(_messages: &[Message]) -> crate::Result<()> {
    todo!("TODO(02)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_jsonl_round_trip() {
        let contents = vec![
            Content::Text("hi".into()),
            Content::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "file1".into(),
                is_error: false,
            },
        ];
        for c in &contents {
            let json = serde_json::to_string(c).unwrap();
            let back: Content = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, c);
        }
    }
}
