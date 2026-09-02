//! 消息模型（第二版 §2.2）。
//!
//! 会话不变量（构造时保证，`validate` 兜底，第二版 §2.2）：
//!
//! 1. user / assistant 严格交替；
//! 2. assistant 的每个 `ToolUse` 在紧接着的 user 消息里有同 id 的 `ToolResult`；
//! 3. 不发空 content；
//! 4. 被打断 / 出错的 tool call 补 `is_error = true` 的结果，
//!    文本 "interrupted by user"。

use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;

use crate::tools::ToolCall;
use crate::tools::ToolOutput;

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

/// 被打断 / 出错的 tool call 的补结果文本（第二版 §2.2 不变量 4）。
pub const INTERRUPTED_TEXT: &str = "interrupted by user";

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Message {
    pub fn user_text(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![Content::Text(text)],
            ts: now_ts(),
            usage: None,
        }
    }

    pub fn assistant(content: Vec<Content>, usage: Option<Usage>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            ts: now_ts(),
            usage,
        }
    }

    /// assistant 消息里的全部 ToolUse，供 loop 逐个审批执行（第二版 §2.5）。
    pub fn tool_uses(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

impl Content {
    /// 不变量第 4 条：被打断 / 出错的 tool call 补错误结果。
    pub fn interrupted(call: &ToolCall) -> Self {
        Self::ToolResult {
            tool_use_id: call.id.clone(),
            content: INTERRUPTED_TEXT.to_string(),
            is_error: true,
        }
    }

    /// 工具执行结果 → ToolResult。
    pub fn tool_result(call: &ToolCall, out: ToolOutput) -> Self {
        Self::ToolResult {
            tool_use_id: call.id.clone(),
            content: out.text,
            is_error: out.is_error,
        }
    }
}

fn tool_use_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

/// 检查四条会话不变量；`16` 在测试里每次 append 后都跑（第二版 §6 风险 2）。
///
/// 不变量 4 是构造约定（`Content::interrupted`），不是校验项；这里校验 1～3，
/// 外加"会话以 user 消息开始"（否则不变量 2 的"紧接着的 user 消息"无从谈起，
/// 且 provider 也不接受以 assistant 开头的对话）。
pub fn validate(messages: &[Message]) -> crate::Result<()> {
    for (i, message) in messages.iter().enumerate() {
        if message.content.is_empty() {
            bail!("invariant 3: message {i} has empty content");
        }
        for (j, block) in message.content.iter().enumerate() {
            if matches!(block, Content::Text(text) if text.is_empty()) {
                bail!("invariant 3: message {i} block {j} is an empty text block");
            }
        }
        if i == 0 {
            if message.role != Role::User {
                bail!("invariant 1: conversation must start with a user message");
            }
        } else if messages[i - 1].role == message.role {
            bail!(
                "invariant 1: message {i} breaks user/assistant alternation (consecutive {:?})",
                message.role
            );
        }
        let uses = tool_use_ids(message);
        if message.role == Role::Assistant && !uses.is_empty() {
            match messages.get(i + 1) {
                None => bail!(
                    "invariant 2: assistant message {i} ends with unanswered tool calls: {:?}",
                    uses
                ),
                Some(next) => {
                    let results = tool_result_ids(next);
                    for id in uses {
                        if !results.contains(&id) {
                            bail!(
                                "invariant 2: tool use {id} in assistant message {i} has no \
                                 matching tool result in message {}",
                                i + 1
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
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

    fn user(content: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content,
            ts: 0,
            usage: None,
        }
    }

    #[test]
    fn valid_conversation_with_tool_roundtrip_passes() {
        let messages = vec![
            Message::user_text("list files".into()),
            Message::assistant(vec![tool_use("t1")], None),
            user(vec![tool_result("t1")]),
            Message::assistant(vec![Content::Text("done".into())], None),
        ];
        validate(&messages).unwrap();
    }

    #[test]
    fn invariant1_consecutive_roles_rejected() {
        let messages = vec![
            Message::user_text("a".into()),
            Message::user_text("b".into()),
        ];
        let err = validate(&messages).unwrap_err().to_string();
        assert!(err.contains("invariant 1"), "{err}");
    }

    #[test]
    fn invariant1_starting_with_assistant_rejected() {
        let messages = vec![Message::assistant(vec![Content::Text("hi".into())], None)];
        let err = validate(&messages).unwrap_err().to_string();
        assert!(err.contains("invariant 1"), "{err}");
    }

    #[test]
    fn invariant2_unanswered_tool_use_rejected() {
        let messages = vec![
            Message::user_text("run".into()),
            Message::assistant(vec![tool_use("t1")], None),
        ];
        let err = validate(&messages).unwrap_err().to_string();
        assert!(err.contains("invariant 2"), "{err}");

        let wrong_id = vec![
            Message::user_text("run".into()),
            Message::assistant(vec![tool_use("t1")], None),
            user(vec![tool_result("t2")]),
        ];
        let err = validate(&wrong_id).unwrap_err().to_string();
        assert!(err.contains("invariant 2"), "{err}");
    }

    #[test]
    fn invariant3_empty_content_rejected() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![],
            ts: 0,
            usage: None,
        }];
        let err = validate(&messages).unwrap_err().to_string();
        assert!(err.contains("invariant 3"), "{err}");

        let empty_text = vec![Message::user_text(String::new())];
        let err = validate(&empty_text).unwrap_err().to_string();
        assert!(err.contains("invariant 3"), "{err}");
    }

    #[test]
    fn invariant4_interrupted_builds_error_tool_result() {
        let call = ToolCall {
            id: "t1".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        };
        let content = Content::interrupted(&call);
        assert_eq!(
            content,
            Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "interrupted by user".into(),
                is_error: true,
            }
        );

        let out = ToolOutput {
            text: "42".into(),
            is_error: false,
        };
        assert_eq!(
            Content::tool_result(&call, out),
            Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "42".into(),
                is_error: false,
            }
        );
    }

    #[test]
    fn tool_uses_extracts_calls_in_order() {
        let message = Message::assistant(
            vec![
                Content::Text("working".into()),
                tool_use("a"),
                tool_use("b"),
            ],
            None,
        );
        let calls = message.tool_uses();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[1].id, "b");
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn summary_message_is_valid_user_message() {
        let messages = vec![Message::user_text(format!(
            "{SUMMARY_PREFIX}\nwe did things"
        ))];
        validate(&messages).unwrap();
    }
}
