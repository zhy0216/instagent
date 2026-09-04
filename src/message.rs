//! 消息模型（第二版 §2.2）。
//!
//! 会话不变量（构造时保证，[`validate`] 兜底，第二版 §2.2 + S8 role/content 矩阵）：
//!
//! 1. user / assistant 严格交替，会话以 user 开始；
//! 2. ToolUse 只在 assistant、ToolResult / Image 只在 user；ToolUse id/name 非空且
//!    全局唯一；紧邻 user 消息的 ToolResult id 多重集与 assistant 的 ToolUse id
//!    多重集精确相等（无 orphan / extra / duplicate）；
//! 3. 不发空 content、空 Text 块；Image 的 media_type、canonical base64 与大小
//!    符合约束（metadata 形状由 serde 的 `ImageData` 钉死）。
//!
//! 统一边界：provider wire（`OpenAiProvider::stream`）、`Session::append` /
//! `rewrite` / salvage 与任何消息入口都调用同一套校验；append 额外容忍
//! "尾部 assistant 的 tool calls 尚未答复"这一 loop 中间态
//! （[`validate_for_append`]，结果由同一次 `finish_turn` 的下一条 append 补上）。

use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;

use crate::tools::ImageData;
use crate::tools::ToolCall;
use crate::tools::ToolOutput;

/// 无 system 角色，system 是请求字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 含 Image（图片支持方案），无 Thinking（v1）。
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
    /// 形状 `{"data": ..., "media_type": ...}`（ImageData）。
    Image(crate::tools::ImageData),
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

pub(crate) fn now_ts() -> i64 {
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

/// 图片解码后字节上限，与 read_image 的 `MAX_IMAGE_BYTES` 同一口径
/// （`src/tools/builtin/image.rs` 私有模块常量，此处独立声明避免反向依赖）。
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// `MAX_IMAGE_BYTES` 对应的 base64 编码长度上限（3 字节 → 4 字符）。
const MAX_IMAGE_B64_LEN: usize = (MAX_IMAGE_BYTES as usize).div_ceil(3) * 4;

/// [`ImageData`] 允许的 MIME（`src/tools.rs` ImageData 文档口径）。
const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// base64 标准字母表（RFC 4648）字符 → 6 位值。
fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 校验 canonical 标准 base64（长度 %4、字母表、`=` 只可结尾且尾组余位必须为 0），
/// 返回解码后字节数；Err 给首个非法字节的偏移（不返回任何原始数据）。
fn check_base64(data: &[u8]) -> std::result::Result<u64, usize> {
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return Err(data.len());
    }
    let groups = data.len() / 4;
    let mut decoded = 0u64;
    for (g, chunk) in data.chunks_exact(4).enumerate() {
        let base = g * 4;
        if g + 1 < groups {
            for (k, c) in chunk.iter().enumerate() {
                if b64_value(*c).is_none() {
                    return Err(base + k);
                }
            }
            decoded += 3;
            continue;
        }
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 {
            return Err(base + 2);
        }
        for (k, c) in chunk[..4 - pad].iter().enumerate() {
            if b64_value(*c).is_none() {
                return Err(base + k);
            }
        }
        match pad {
            1 => {
                let v = b64_value(chunk[2]).unwrap_or(0);
                if v & 0b11 != 0 {
                    return Err(base + 2);
                }
                decoded += 2;
            }
            2 => {
                let v = b64_value(chunk[1]).unwrap_or(0);
                if v & 0b1111 != 0 {
                    return Err(base + 1);
                }
                decoded += 1;
            }
            _ => decoded += 3,
        }
    }
    Ok(decoded)
}

fn validate_image(i: usize, j: usize, image: &ImageData) -> crate::Result<()> {
    if !IMAGE_MEDIA_TYPES.contains(&image.media_type.as_str()) {
        bail!(
            "image constraint: message {i} block {j}: media_type {:?} is not one of \
             {IMAGE_MEDIA_TYPES:?}",
            image.media_type
        );
    }
    if image.data.len() > MAX_IMAGE_B64_LEN {
        bail!(
            "image constraint: message {i} block {j}: base64 payload of {} chars exceeds the \
             {MAX_IMAGE_BYTES} decoded-byte limit",
            image.data.len()
        );
    }
    match check_base64(image.data.as_bytes()) {
        Ok(decoded) => {
            if decoded > MAX_IMAGE_BYTES {
                bail!(
                    "image constraint: message {i} block {j}: {decoded} decoded bytes exceed \
                     the {MAX_IMAGE_BYTES} byte limit"
                );
            }
            Ok(())
        }
        Err(offset) => bail!(
            "image constraint: message {i} block {j}: data is not canonical standard-alphabet \
             base64 (invalid byte at offset {offset})"
        ),
    }
}

/// 错误文案里 id 列表的展示上限（防坏输入把整条消息的 id 刷进日志）。
fn bounded_ids(ids: &[&str]) -> String {
    const SHOW: usize = 8;
    let head: Vec<String> = ids.iter().take(SHOW).map(|id| format!("{id:?}")).collect();
    if ids.len() > SHOW {
        format!("{} (+{} more)", head.join(", "), ids.len() - SHOW)
    } else {
        head.join(", ")
    }
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

/// role/content 矩阵 + 会话不变量的统一校验核心（S8）。
fn validate_with(messages: &[Message], allow_open_tool_turn: bool) -> crate::Result<()> {
    // 全局 ToolUse id → 首次出现的消息索引。
    let mut use_owners: Vec<(&str, usize)> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        if message.content.is_empty() {
            bail!("invariant 3: message {i} has empty content");
        }
        let mut uses: Vec<&str> = Vec::new();
        let mut first_result_block: Option<usize> = None;
        for (j, block) in message.content.iter().enumerate() {
            match block {
                Content::Text(text) => {
                    if text.is_empty() {
                        bail!("invariant 3: message {i} block {j} is an empty text block");
                    }
                }
                Content::ToolUse { id, name, .. } => {
                    if message.role != Role::Assistant {
                        bail!(
                            "role matrix: message {i} block {j} is a ToolUse in a {:?} message \
                             (tool calls are only allowed in assistant messages)",
                            message.role
                        );
                    }
                    if id.is_empty() {
                        bail!("invariant 2: message {i} block {j} is a ToolUse with an empty id");
                    }
                    if name.is_empty() {
                        bail!(
                            "invariant 2: message {i} block {j} is a ToolUse with an empty name \
                             (id {id:?})"
                        );
                    }
                    if let Some((_, first)) = use_owners.iter().find(|(prev, _)| *prev == id) {
                        bail!(
                            "invariant 2: message {i} block {j} reuses tool use id {id:?} \
                             (first seen in message {first})"
                        );
                    }
                    use_owners.push((id, i));
                    uses.push(id);
                }
                Content::ToolResult { tool_use_id, .. } => {
                    if message.role != Role::User {
                        bail!(
                            "role matrix: message {i} block {j} is a ToolResult in a {:?} \
                             message (tool results are only allowed in user messages)",
                            message.role
                        );
                    }
                    if tool_use_id.is_empty() {
                        bail!(
                            "invariant 2: message {i} block {j} is a ToolResult with an empty \
                             tool_use_id"
                        );
                    }
                    first_result_block.get_or_insert(j);
                }
                Content::Image(image) => {
                    if message.role != Role::User {
                        bail!(
                            "role matrix: message {i} block {j} is an Image in a {:?} message \
                             (images are only allowed in user messages)",
                            message.role
                        );
                    }
                    validate_image(i, j, image)?;
                }
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
        if message.role == Role::Assistant && !uses.is_empty() {
            match messages.get(i + 1) {
                None if allow_open_tool_turn => {}
                None => bail!(
                    "invariant 2: assistant message {i} ends with unanswered tool calls: {}",
                    bounded_ids(&uses)
                ),
                Some(next) => {
                    let results = tool_result_ids(next);
                    let mut want = uses.clone();
                    want.sort_unstable();
                    let mut got = results;
                    got.sort_unstable();
                    if want != got {
                        let unique: Vec<&str> = {
                            let mut v = got.clone();
                            v.dedup();
                            v
                        };
                        let missing: Vec<&str> = want
                            .iter()
                            .copied()
                            .filter(|id| !unique.contains(id))
                            .collect();
                        let unexpected: Vec<&str> = unique
                            .iter()
                            .copied()
                            .filter(|id| !want.contains(id))
                            .collect();
                        let mut duplicated: Vec<&str> = got
                            .windows(2)
                            .filter(|w| w[0] == w[1])
                            .map(|w| w[0])
                            .collect();
                        duplicated.dedup();
                        let mut parts = Vec::new();
                        if !missing.is_empty() {
                            parts.push(format!("missing results: {}", bounded_ids(&missing)));
                        }
                        if !unexpected.is_empty() {
                            parts.push(format!("unexpected results: {}", bounded_ids(&unexpected)));
                        }
                        if !duplicated.is_empty() {
                            parts.push(format!("duplicated results: {}", bounded_ids(&duplicated)));
                        }
                        bail!(
                            "invariant 2: tool results in message {} do not exactly match the \
                             tool uses of assistant message {i} ({})",
                            i + 1,
                            parts.join("; ")
                        );
                    }
                }
            }
        }
        if let Some(rb) = first_result_block {
            if i == 0
                || messages[i - 1].role != Role::Assistant
                || tool_use_ids(&messages[i - 1]).is_empty()
            {
                let prev = if i == 0 {
                    "there is no previous message".to_string()
                } else {
                    format!("assistant message {} declares no tool uses", i - 1)
                };
                bail!("invariant 2: message {i} block {rb} carries orphan tool results ({prev})");
            }
        }
    }
    Ok(())
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

/// 严格校验整个会话（S8 role/content 矩阵 + 不变量 1～3）。
///
/// 统一消息边界：provider wire（`OpenAiProvider::stream`，proxy 引擎经由它）、
/// `Session::rewrite` 与 resume salvage 都调用本函数；`Session::append` 调用
/// [`validate_for_append`]（同一核心，仅多容忍 loop 中间态）。不变量 4 是构造
/// 约定（`Content::interrupted`），不是校验项。错误带消息索引 / block 索引 /
/// 约束名，不回显消息原文。
pub fn validate(messages: &[Message]) -> crate::Result<()> {
    validate_with(messages, false)
}

/// `Session::append` 边界（`crate::session`）：与 [`validate`] 同一校验核心，
/// 只额外允许"最后一条 assistant 的 ToolUse 尚无结果"——`finish_turn` 先 append
/// assistant、紧接着 append 答复它的 user 结果，两条之间的中间态合法；一旦继续
/// 追加别的消息，[`validate`] 的精确配对即恢复生效。
pub(crate) fn validate_for_append(messages: &[Message]) -> crate::Result<()> {
    validate_with(messages, true)
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
            Content::Image(crate::tools::ImageData {
                data: "aGVsbG8=".into(),
                media_type: "image/png".into(),
            }),
        ];
        for c in &contents {
            let json = serde_json::to_string(c).unwrap();
            let back: Content = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, c);
        }
        assert_eq!(
            serde_json::to_string(&contents[3]).unwrap(),
            r#"{"data":"aGVsbG8=","media_type":"image/png"}"#
        );
    }

    #[test]
    fn untagged_old_shapes_do_not_match_image() {
        let old: Vec<(Content, &str)> = vec![
            (Content::Text("hi".into()), r#""hi""#),
            (
                Content::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
                r#"{"id":"t1","name":"shell","input":{"command":"ls"}}"#,
            ),
            (
                Content::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file1".into(),
                    is_error: false,
                },
                r#"{"tool_use_id":"t1","content":"file1","is_error":false}"#,
            ),
        ];
        for (expected, json) in old {
            let parsed: Content = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected, "{json} 不应误配为 Image");
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
            image: None,
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

    fn image(data: &str, media_type: &str) -> Content {
        Content::Image(ImageData {
            data: data.into(),
            media_type: media_type.into(),
        })
    }

    fn one_turn(after_use: Vec<Content>) -> Vec<Message> {
        vec![
            Message::user_text("run".into()),
            Message::assistant(vec![tool_use("a")], None),
            user(after_use),
        ]
    }

    #[test]
    fn role_matrix_rejects_blocks_in_wrong_roles() {
        let err = validate(&[
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("sure".into())], None),
            user(vec![tool_use("a")]),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("role matrix"), "{err}");
        assert!(err.contains("message 2 block 0"), "{err}");

        let err = validate(&[Message::assistant(vec![tool_result("a")], None)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("role matrix"), "{err}");

        let err = validate(&[
            Message::user_text("hi".into()),
            Message::assistant(vec![image("aGVsbG8=", "image/png")], None),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("role matrix"), "{err}");
        assert!(err.contains("message 1 block 0"), "{err}");
    }

    #[test]
    fn tool_use_ids_and_names_must_be_nonempty_and_unique() {
        let empty_id = vec![
            Message::user_text("run".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "".into(),
                    name: "shell".into(),
                    input: serde_json::json!({}),
                }],
                None,
            ),
        ];
        let err = validate(&empty_id).unwrap_err().to_string();
        assert!(err.contains("empty id"), "{err}");

        let empty_name = vec![
            Message::user_text("run".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "a".into(),
                    name: "".into(),
                    input: serde_json::json!({}),
                }],
                None,
            ),
        ];
        let err = validate(&empty_name).unwrap_err().to_string();
        assert!(err.contains("empty name"), "{err}");

        let empty_result_id = vec![
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("ok".into())], None),
            user(vec![Content::ToolResult {
                tool_use_id: "".into(),
                content: "x".into(),
                is_error: false,
            }]),
        ];
        let err = validate(&empty_result_id).unwrap_err().to_string();
        assert!(err.contains("empty tool_use_id"), "{err}");

        let reused = vec![
            Message::user_text("run".into()),
            Message::assistant(vec![tool_use("a")], None),
            user(vec![tool_result("a")]),
            Message::assistant(vec![tool_use("a")], None),
            user(vec![tool_result("a")]),
        ];
        let err = validate(&reused).unwrap_err().to_string();
        assert!(err.contains("reuses tool use id"), "{err}");
        assert!(err.contains("first seen in message 1"), "{err}");
    }

    #[test]
    fn orphan_extra_and_duplicate_results_rejected() {
        // orphan：user 结果前面是没有 ToolUse 的 assistant。
        let orphan = vec![
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("done".into())], None),
            user(vec![tool_result("a")]),
        ];
        let err = validate(&orphan).unwrap_err().to_string();
        assert!(err.contains("orphan tool results"), "{err}");
        assert!(err.contains("message 2 block 0"), "{err}");

        // orphan：首条 user 就带结果。
        let first = vec![user(vec![tool_result("a")])];
        let err = validate(&first).unwrap_err().to_string();
        assert!(err.contains("orphan tool results"), "{err}");

        // extra：结果比 use 多。
        let extra = one_turn(vec![tool_result("a"), tool_result("b")]);
        let err = validate(&extra).unwrap_err().to_string();
        assert!(err.contains("unexpected results"), "{err}");
        assert!(err.contains("\"b\""), "{err}");

        // duplicate：同 id 结果出现两次。
        let dup = one_turn(vec![tool_result("a"), tool_result("a")]);
        let err = validate(&dup).unwrap_err().to_string();
        assert!(err.contains("duplicated results"), "{err}");

        // 精确相等允许文本块混排（round-trip 合法形状）。
        validate(&one_turn(vec![
            tool_result("a"),
            Content::Text("more".into()),
        ]))
        .unwrap();
    }

    /// 图片块放在合法位置（交替后的 user 消息）的会话样本。
    fn image_case(data: &str, media_type: &str) -> Vec<Message> {
        vec![
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("see".into())], None),
            user(vec![image(data, media_type)]),
        ]
    }

    #[test]
    fn image_constraints_reject_bad_mime_base64_and_size() {
        let err = validate(&image_case("aGVsbG8=", "text/plain"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("media_type"), "{err}");
        // 错误文案只回显短 media_type，不回显 data。
        assert!(!err.contains("aGVsbG8="), "{err}");

        let err = validate(&image_case("aGVs bG8=", "image/png"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64"), "{err}");
        assert!(!err.contains("aGVs"), "{err}");

        let err = validate(&image_case("aGVsbG9=", "image/png"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64"), "{err}");

        assert!(validate(&image_case("aGVsbG8", "image/png")).is_err());
        assert!(validate(&image_case("", "image/png")).is_err());

        let err = validate(&image_case(&"A".repeat(MAX_IMAGE_B64_LEN + 4), "image/png"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");

        // 合法四格式 + canonical base64（含两级 padding）通过。
        for mt in IMAGE_MEDIA_TYPES {
            for data in ["Zg==", "Zm8=", "Zm9v"] {
                validate(&image_case(data, mt)).unwrap();
            }
        }
    }

    #[test]
    fn check_base64_matches_encoder_vectors() {
        for (case, decoded) in [
            ("Zg==", 1),
            ("Zm8=", 2),
            ("Zm9v", 3),
            ("Zm9vYg==", 4),
            ("Zm9vYmE=", 5),
            ("Zm9vYmFy", 6),
        ] {
            assert_eq!(check_base64(case.as_bytes()).unwrap(), decoded, "{case}");
        }
        assert!(check_base64(b"").is_err());
        assert!(check_base64(b"Zm9=").is_err(), "尾组非 canonical");
        assert!(check_base64(b"=== ").is_err());
    }

    #[test]
    fn valid_text_tool_image_roundtrip_still_passes() {
        let messages = vec![
            Message::user_text("look".into()),
            Message::assistant(vec![Content::Text("run".into()), tool_use("t1")], None),
            user(vec![
                tool_result("t1"),
                image(PNG_B64, "image/png"),
                Content::Text("and this?".into()),
            ]),
            Message::assistant(vec![Content::Text("a png".into())], None),
        ];
        validate(&messages).unwrap();
    }

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn validate_for_append_allows_only_the_open_tool_turn_tail() {
        let open = vec![
            Message::user_text("run".into()),
            Message::assistant(vec![tool_use("a")], None),
        ];
        validate(&open).unwrap_err();
        validate_for_append(&open).unwrap();

        // 中间态之后继续追加不配对的 user 文本 → 依然拒绝。
        let mut continued = open.clone();
        continued.push(Message::user_text("changed my mind".into()));
        assert!(validate_for_append(&continued).is_err());

        // 补上精确配对的结果后，两种口径都通过。
        let mut closed = open;
        closed.push(user(vec![tool_result("a")]));
        validate_for_append(&closed).unwrap();
        validate(&closed).unwrap();

        // append 边界不放松其它约束：orphan 结果照样拒绝。
        let orphan = vec![
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("ok".into())], None),
            user(vec![tool_result("ghost")]),
        ];
        assert!(validate_for_append(&orphan).is_err());
    }
}
