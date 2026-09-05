//! 自动压缩（第二版 §2.7）。触发：每条 assistant 消息后
//! `usage.input >= threshold × context_limit`；或由库调用者显式触发；
//! 或 provider 报 ContextOverflow（强制压缩后重试一次）。不引 tokenizer。
//!
//! 摘要请求 = 历史格式化成文本（>2KB 的 ToolResult 正文先替换成
//! `[truncated N bytes]`，防摘要器自身溢出）+ [`COMPACTION_PROMPT`]。
//! 结果：历史替换为一条 [`SUMMARY_PREFIX`] 开头的 User 消息（末尾未回答的
//! user 消息合并保留），经 `02` 的原子重写落盘，发 [`Event::Compacted`]。

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::event::Event;
use crate::agent::prompt;
use crate::agent::prompt::PromptContext;
use crate::agent::Agent;
use crate::message::Content;
use crate::message::Message;
use crate::message::Role;
use crate::message::Usage;
use crate::message::SUMMARY_PREFIX;
use crate::provider::Request;
use crate::provider::StreamEvent;
use crate::session::Session;

/// 摘要 prompt：文本搬运自 goose
/// `crates/goose-context-management/src/prompts/compaction.md`（commit `4ad43df`）。
/// Task Context 与字段规则近乎逐字（user_intent / files / errors_and_fixes /
/// pending_tasks / current_work 等）；仅把"goose 输出 JSON 再由模板渲染"改成
/// "直接输出 markdown"（第二版 §2.7：v1 不做 JSON + 模板层）。
pub const COMPACTION_PROMPT: &str = r##"## Task Context
- An llm context limit was reached when a user was in a working session with an agent (you)
- Distill the conversation below into a structured summary with only the most verbose parts removed
- Include user requests, your responses, all technical content, and as much of the original context as possible
- This will be used to let the user continue the working session
- The summary will be read by an agent (you) on a next exchange to allow for continuation of the session

**Conversation History:**
{{ messages }}

Write the summary as a Markdown document with sections named after the fields below.
Order every list from most to least important, and omit a section rather than
inventing content for it:

- user_intent: every user goal and request, most important first
- technical_concepts: all discussed tools, methods, and concepts
- files: for each file viewed or edited - its path, what was done to it and why,
  and important code, signatures, or diffs (omit if none)
- errors_and_fixes: bugs hit, their resolutions, and user-driven changes
- problem_solving: issues solved or in progress, and key decisions: what was
  chosen, what was rejected, and why
- user_messages: all user messages, truncating long tool call arguments or results
- pending_tasks: all unresolved user requests, most important first
- current_work: active work at summary request time: filenames, code, alignment
  to latest instruction
- next_step: include only if it directly continues a user instruction, otherwise omit

Rules for the summary:
- Quote error messages, panic text, and failing test output verbatim in
  errors_and_fixes - exact strings including numbers, identifiers, and paths,
  not paraphrases
- This summary will only be read by you, so it is ok to make it much longer than
  a normal summary you would show to a human: quote liberally - full output
  blocks, complete code snippets, exact user wording
- Do not exclude any information that might be important to continuing a session
  working with you
- No new ideas unless user confirmed
"##;

/// [`COMPACTION_PROMPT`] 里历史文本的占位符（goose 模板同名）。
pub const MESSAGES_PLACEHOLDER: &str = "{{ messages }}";

/// 超过此字节数的 ToolResult 正文先截断再送摘要器。
pub const TOOL_RESULT_TRUNCATE_BYTES: usize = 2048;

/// 阈值判定（threshold 默认 0.8）。floor 消除 f32·u32 的浮点尾差，
/// 让"恰好到阈值"稳定命中。
pub fn should_compact(usage: &Usage, context_limit: u32, threshold: f32) -> bool {
    f64::from(usage.input) >= (f64::from(context_limit) * f64::from(threshold)).floor()
}

/// 每轮开头的条件压缩；发生则返回 true 并发 Event::Compacted。
///
/// 兼容包装（取消语义见 [`maybe_cancelable`]）：用永不取消的 token 调用，
/// 行为与旧 `maybe` 一致。`08` 接线 CLI 取消时请用 [`maybe_cancelable`]。
pub async fn maybe(
    agent: &Agent,
    session: &mut Session,
    events: &mpsc::Sender<Event>,
) -> crate::Result<bool> {
    maybe_cancelable(agent, session, events, &CancellationToken::new()).await
}

/// ContextOverflow / `/compact` 的强制压缩。
///
/// 兼容包装（取消语义见 [`force_cancelable`]）：用永不取消的 token 调用，
/// 行为与旧 `force` 一致（摘要空/缺 Done 照样报错、不改会话）。
/// `08` 接线 CLI 取消时请用 [`force_cancelable`]。
pub async fn force(
    agent: &Agent,
    session: &mut Session,
    events: &mpsc::Sender<Event>,
) -> crate::Result<()> {
    force_cancelable(agent, session, events, &CancellationToken::new())
        .await
        .map(|_| ())
}

/// 可取消的条件压缩（供 `08` 接线 CLI 取消）：
/// - 取消时不改会话、不发事件，返回 `Ok(false)`；调用方应随后检查 token
///   并按 `Interrupted` 收尾（`run_turn` 已如此处理）；
/// - 阈值未命中返回 `Ok(false)`；发生压缩返回 `Ok(true)`；
/// - 摘要失败（提供方错误 / 缺 Done / 空文本）返回 `Err`，同样不改会话。
pub async fn maybe_cancelable(
    agent: &Agent,
    session: &mut Session,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> crate::Result<bool> {
    let Some(usage) = last_usage_needing_compaction(session) else {
        return Ok(false);
    };
    if !should_compact(
        &usage,
        agent.cfg.context_limit,
        agent.cfg.compaction_threshold,
    ) {
        return Ok(false);
    }
    force_cancelable(agent, session, events, cancel).await
}

/// 可取消的强制压缩（供 `08` 接线 CLI 取消），返回是否发生了 `rewrite`：
/// - 取消时不改会话、不发事件，返回 `Ok(false)`；调用方用返回值或 token
///   区分"取消的 no-op"与"成功的压缩"（`run_turn` 在调用后检查 token）；
/// - 摘要成功后才 `rewrite`（`Ok(true)`）：`summarize` 只接受非空且协议完成
///   （收到 Done）的输出，空/断流/取消一律不替换历史；
/// - 取消是调用方动作，不报"provider 截断"，不发 `Event::Error`。
pub async fn force_cancelable(
    agent: &Agent,
    session: &mut Session,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> crate::Result<bool> {
    let Some((head, tail)) = split_head_tail(session) else {
        return Ok(false);
    };
    let before_tokens = head.iter().rev().find_map(|message| match message {
        Message {
            role: Role::Assistant,
            usage: Some(usage),
            ..
        } => Some(usage.input),
        _ => None,
    });

    let history = format_history(&head);
    // 取消 → Ok(false) 做 no-op（调用方看返回值/token）；失败 → Err 且不改会话。
    let Some((summary, usage)) = summarize(agent, session, &history, cancel).await? else {
        return Ok(false);
    };

    let mut text = format!("{SUMMARY_PREFIX}\n{summary}");
    if let Some(tail) = tail {
        text.push_str("\n\nLatest unanswered user message:\n");
        text.push_str(&tail);
    }
    session.rewrite(vec![Message::user_text(text)])?;
    crate::agent::event::emit(
        events,
        Event::Compacted {
            before_tokens: before_tokens.unwrap_or(0),
            // v1 不引 tokenizer：压缩后的上下文体量用摘要响应的 output 近似。
            after_tokens: usage.output,
        },
    )
    .await;
    Ok(true)
}

/// 需要摘要的历史 + 末尾未回答的 user 文本（合并保留用）。
/// 少于两条消息（没有可压缩的历史）时返回 None。
fn split_head_tail(session: &Session) -> Option<(Vec<Message>, Option<String>)> {
    if session.messages.len() < 2 {
        return None;
    }
    let last = session.messages.last()?;
    if last.role == Role::User {
        // 工具结果 user 消息是对 assistant 消息的答复，不作为 tail 保留。
        let answered = last
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolResult { .. }));
        if !answered {
            if let Content::Text(text) = &last.content.first()? {
                return Some((
                    session.messages[..session.messages.len() - 1].to_vec(),
                    Some(text.clone()),
                ));
            }
        }
    }
    Some((session.messages.clone(), None))
}

/// 最近一次"还没被压缩覆盖"的 assistant usage；末尾 summary 之后的才有意义。
fn last_usage_needing_compaction(session: &Session) -> Option<Usage> {
    let summary_at = session.messages.iter().rposition(is_summary_message);
    let start = summary_at.map_or(0, |index| index + 1);
    session.messages[start..]
        .iter()
        .rev()
        .find_map(|message| match message {
            Message {
                role: Role::Assistant,
                usage: Some(usage),
                ..
            } => Some(*usage),
            _ => None,
        })
}

fn is_summary_message(message: &Message) -> bool {
    message.role == Role::User
        && matches!(
            message.content.first(),
            Some(Content::Text(text)) if text.starts_with(SUMMARY_PREFIX)
        )
}

/// 历史 → 纯文本（ToolResult 正文 >2KB 先截断）。
pub(crate) fn format_history(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &message.content {
            match block {
                Content::Text(text) => out.push_str(&format!("{role}: {text}\n")),
                Content::ToolUse { id, name, input } => {
                    out.push_str(&format!("assistant tool_use {name} ({id}): {input}\n"));
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let body = truncate_tool_result(content);
                    out.push_str(&format!(
                        "tool_result {tool_use_id} (error={is_error}): {body}\n"
                    ));
                }
                Content::Image(img) => out.push_str(&format!(
                    "{role}: [image: {}, {} base64 bytes omitted]\n",
                    img.media_type,
                    img.data.len()
                )),
            }
        }
    }
    out
}

fn truncate_tool_result(content: &str) -> String {
    if content.len() > TOOL_RESULT_TRUNCATE_BYTES {
        format!("[truncated {} bytes]", content.len())
    } else {
        content.to_string()
    }
}

/// 发一次不带 tools 的摘要请求，折叠成完整文本。
///
/// - `Ok(Some)`：收到 Done 且文本非空，调用方可 rewrite；
/// - `Ok(None)`：等待中被取消，调用方按 no-op 处理（不 rewrite、不报截断）；
/// - `Err`：提供方错误 / Done 前结束 / 空摘要，一律不 rewrite（A04：无内容或
///   异常摘要不得覆盖历史；取消不误报成 provider 截断）。
async fn summarize(
    agent: &Agent,
    session: &Session,
    history: &str,
    cancel: &CancellationToken,
) -> crate::Result<Option<(String, Usage)>> {
    let user = Message::user_text(COMPACTION_PROMPT.replace(MESSAGES_PLACEHOLDER, history));
    let ctx = PromptContext {
        tools: &[],
        cwd: &session.header.cwd,
        now: chrono::Utc::now(),
        mcp_instructions: &agent.mcp_instructions,
        skill_lines: &agent.skill_lines,
    };
    let system = prompt::system(&ctx);
    let request = Request {
        model: &agent.cfg.model,
        system: &system,
        messages: std::slice::from_ref(&user),
        tools: &[],
        max_tokens: agent.cfg.max_tokens,
        temperature: None,
    };
    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(None),
        stream = agent.provider.stream(request) => stream?,
    };
    let mut text = String::new();
    let mut usage = Usage::default();
    let mut done = false;
    while let Some(event) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(None),
        event = stream.next() => event,
    } {
        match event? {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::Done { usage: u, .. } => {
                usage = u;
                done = true;
            }
            _ => {}
        }
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if !done {
        anyhow::bail!("summarization stream ended without Done; keeping the original session");
    }
    if text.trim().is_empty() {
        anyhow::bail!("summarization returned empty text; keeping the original session");
    }
    Ok(Some((text, usage)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compact_uses_threshold_product() {
        let usage = Usage {
            input: 800,
            ..Default::default()
        };
        assert!(should_compact(&usage, 1000, 0.8));
        assert!(!should_compact(
            &Usage {
                input: 799,
                ..Default::default()
            },
            1000,
            0.8
        ));
        assert!(!should_compact(&usage, 10_000, 0.8));
    }

    #[test]
    fn compaction_prompt_keeps_goose_task_context_and_fields() {
        assert!(COMPACTION_PROMPT.contains(MESSAGES_PLACEHOLDER));
        for field in [
            "user_intent",
            "errors_and_fixes",
            "pending_tasks",
            "current_work",
        ] {
            assert!(COMPACTION_PROMPT.contains(field), "{field}");
        }
    }

    #[test]
    fn oversized_tool_results_are_truncated_for_the_summarizer() {
        let big = "x".repeat(TOOL_RESULT_TRUNCATE_BYTES + 1000);
        let messages = vec![
            Message::user_text("go".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "t1".into(),
                    content: big.clone(),
                    is_error: false,
                }],
                ts: 0,
                usage: None,
            },
        ];
        let history = format_history(&messages);
        assert!(
            history.contains(&format!("[truncated {} bytes]", big.len())),
            "{history}"
        );
        assert!(!history.contains(&"x".repeat(3000)));
        // 未超限的正文原样保留。
        assert!(history.contains("tool_result t1 (error=false)"));
    }

    #[test]
    fn images_become_placeholders_never_base64_in_history() {
        let mut data = String::new();
        for byte in 0..255u8 {
            data.push_str(&format!("{byte:02x}"));
        }
        let img = crate::tools::ImageData {
            data,
            media_type: "image/png".into(),
        };
        let len = img.data.len();
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "read_image ok".into(),
                    is_error: false,
                },
                Content::Image(img.clone()),
            ],
            ts: 0,
            usage: None,
        }];
        let out = format_history(&messages);
        assert!(
            out.contains(&format!("[image: image/png, {len} base64 bytes omitted]")),
            "{out}"
        );
        assert!(!out.contains(&img.data));
    }

    #[test]
    fn split_head_tail_keeps_only_unanswered_trailing_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session {
            header: crate::session::SessionHeader {
                id: "s".into(),
                created: 0,
                cwd: dir.path().to_path_buf(),
                provider: "mock".into(),
                model: "m".into(),
            },
            messages: vec![
                Message::user_text("first".into()),
                Message::assistant(vec![Content::Text("ok".into())], None),
                Message::user_text("follow-up".into()),
            ],
            path: dir.path().join("s.jsonl"),
        };
        let (head, tail) = split_head_tail(&session).unwrap();
        assert_eq!(head.len(), 2);
        assert_eq!(tail.as_deref(), Some("follow-up"));

        // 末尾是工具结果（已回答）：全部进 head，无 tail。
        session
            .messages
            .push(Message::assistant(vec![Content::Text("done".into())], None));
        session.messages.push(Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "out".into(),
                is_error: false,
            }],
            ts: 0,
            usage: None,
        });
        let (head, tail) = split_head_tail(&session).unwrap();
        assert_eq!(head.len(), session.messages.len());
        assert_eq!(tail, None);
    }
}
