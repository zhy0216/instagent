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
use crate::provider::StopReason;
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
/// - 摘要失败（提供方错误 / 异常终止 / 工具事件 / 缺 Done / 空文本）返回
///   `Err`，同样不改会话。
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
/// - 摘要成功后才 `rewrite`（`Ok(true)`）：`summarize` 只接受非空纯文本且
///   正常结束（Done / EndTurn）的输出，空/异常/断流/取消一律不替换历史；
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
    let mut content = tail.unwrap_or_default();
    if !content.is_empty() {
        text.push_str("\n\nLatest unanswered user message:\n");
    }
    // 首个 Text 延续既有的摘要 + 输入形式；其余块保持原样与原顺序。
    // 图片开头的输入在前面新增摘要 Text，图片本身不转成文本或丢弃。
    match content.first_mut() {
        Some(Content::Text(first)) => first.insert_str(0, &text),
        _ => content.insert(0, Content::Text(text)),
    }
    session.rewrite(vec![Message {
        role: Role::User,
        content,
        ts: crate::message::now_ts(),
        usage: None,
    }])?;
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

/// 需要摘要的历史 + 末尾未回答的 user 内容块（按原顺序合并保留）。
/// 少于两条消息（没有可压缩的历史）时返回 None。
fn split_head_tail(session: &Session) -> Option<(Vec<Message>, Option<Vec<Content>>)> {
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
            return Some((
                session.messages[..session.messages.len() - 1].to_vec(),
                Some(last.content.clone()),
            ));
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
/// - `Ok(Some)`：非空纯文本以唯一的 Done / EndTurn 正常结束，调用方可 rewrite；
/// - `Ok(None)`：等待中被取消，调用方按 no-op 处理（不 rewrite、不报截断）；
/// - `Err`：提供方错误 / 异常终止 / 工具事件 / Done 前结束或其后仍有事件 /
///   空摘要，一律不 rewrite（无内容或异常摘要不得覆盖历史）。
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
    let mut usage = None;
    while let Some(event) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(None),
        event = stream.next() => event,
    } {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let event = event?;
        if usage.is_some() {
            anyhow::bail!(
                "summarization returned an event after Done; keeping the original session"
            );
        }
        match event {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::Done {
                usage: u,
                stop_reason,
            } => {
                if stop_reason != StopReason::EndTurn {
                    anyhow::bail!(
                        "summarization ended with {stop_reason:?}, expected EndTurn; \
                         keeping the original session"
                    );
                }
                usage = Some(u);
            }
            StreamEvent::ToolUseStart { .. }
            | StreamEvent::ToolUseDelta(_)
            | StreamEvent::ToolUseEnd => {
                anyhow::bail!(
                    "summarization returned an unexpected tool event; keeping the original session"
                );
            }
        }
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let Some(usage) = usage else {
        anyhow::bail!("summarization stream ended without Done; keeping the original session");
    };
    if text.trim().is_empty() {
        anyhow::bail!("summarization returned empty text; keeping the original session");
    }
    Ok(Some((text, usage)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentCfg;
    use crate::agent::TurnResult;
    use crate::provider::Provider;
    use crate::tools::ImageData;
    use crate::tools::Registry;
    use crate::ProviderError;
    use async_trait::async_trait;
    use futures::stream;
    use futures::stream::BoxStream;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::Notify;

    const SUMMARY_TEXT: &str = "## user_intent\nsummarized old work";
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    /// Session::resume 按进程环境定位数据目录。每个行为测试独占子进程，
    /// 只向它注入临时目录，不修改父进程环境、不与其它模块的 env 测试争用。
    async fn isolated(test: &str) -> bool {
        const CHILD_ENV: &str = "INSTAGENT_COMPACTION_TEST_CHILD";
        if std::env::var(CHILD_ENV).as_deref() == Ok(test) {
            return false;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &format!("agent::compact::tests::{test}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, test)
            .env("INSTAGENT_DATA_DIR", dir.path())
            .env_remove("TOKEN_PLAN_API_KEY")
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let output = tokio::time::timeout(Duration::from_secs(60), command.output())
            .await
            .expect("isolated compaction test timed out")
            .unwrap();
        assert!(
            output.status.success(),
            "{test}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    enum Response {
        Events(Vec<Result<StreamEvent, ProviderError>>),
        Error(ProviderError),
        PendingConnect(Arc<Notify>),
        PendingAfter(Vec<StreamEvent>, Arc<Notify>),
        CancelAtEof(Vec<StreamEvent>, CancellationToken),
    }

    impl Response {
        fn events(events: Vec<StreamEvent>) -> Self {
            Self::Events(events.into_iter().map(Ok).collect())
        }
    }

    struct MockProvider {
        responses: Mutex<VecDeque<Response>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Response>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn stream(
            &self,
            request: Request<'_>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            crate::message::validate(request.messages).unwrap();
            assert!(
                request.tools.is_empty(),
                "summary requests cannot offer tools"
            );
            self.seen.lock().unwrap().push(request.messages.to_vec());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected provider request");
            match response {
                Response::Events(events) => Ok(stream::iter(events).boxed()),
                Response::Error(error) => Err(error),
                Response::PendingConnect(entered) => {
                    entered.notify_one();
                    std::future::pending().await
                }
                Response::PendingAfter(events, entered) => {
                    let pending = stream::once(async move {
                        entered.notify_one();
                        std::future::pending().await
                    });
                    Ok(stream::iter(events.into_iter().map(Ok))
                        .chain(pending)
                        .boxed())
                }
                Response::CancelAtEof(events, token) => {
                    let eof = stream::poll_fn(move |_| {
                        token.cancel();
                        Poll::Ready(None)
                    });
                    Ok(stream::iter(events.into_iter().map(Ok)).chain(eof).boxed())
                }
            }
        }
    }

    fn agent(provider: Arc<dyn Provider>) -> Agent {
        Agent {
            cfg: AgentCfg {
                model: "mock-model".into(),
                max_tokens: 1024,
                max_turns: 4,
                context_limit: 1000,
                compaction_threshold: 0.8,
            },
            provider,
            tools: Arc::new(Registry::new()),
            hooks: None,
            mcp_instructions: Vec::new(),
            skill_lines: Vec::new(),
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Entry {
        Maybe,
        Force,
        MaybeCancelable,
        ForceCancelable,
    }

    impl Entry {
        const ALL: [Self; 4] = [
            Self::Maybe,
            Self::Force,
            Self::MaybeCancelable,
            Self::ForceCancelable,
        ];

        async fn apply(
            self,
            agent: &Agent,
            session: &mut Session,
            tx: &mpsc::Sender<Event>,
        ) -> crate::Result<()> {
            let token = CancellationToken::new();
            match self {
                Self::Maybe => maybe(agent, session, tx).await.map(|_| ()),
                Self::Force => force(agent, session, tx).await,
                Self::MaybeCancelable => maybe_cancelable(agent, session, tx, &token)
                    .await
                    .map(|_| ()),
                Self::ForceCancelable => force_cancelable(agent, session, tx, &token)
                    .await
                    .map(|_| ()),
            }
        }
    }

    fn done(stop_reason: StopReason) -> StreamEvent {
        StreamEvent::Done {
            usage: Usage {
                input: 100,
                output: 30,
                ..Default::default()
            },
            stop_reason,
        }
    }

    fn summary_events() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta("## user_intent\n".into()),
            StreamEvent::TextDelta("summarized old work".into()),
            done(StopReason::EndTurn),
        ]
    }

    fn user(content: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content,
            ts: 42,
            usage: None,
        }
    }

    fn png() -> Content {
        Content::Image(ImageData {
            data: PNG_B64.into(),
            media_type: "image/png".into(),
        })
    }

    fn unanswered_inputs() -> Vec<Vec<Content>> {
        vec![
            vec![Content::Text("pending single input".into())],
            vec![
                Content::Text("pending first input\n原始任务".into()),
                Content::Text("pending second input\n恢复补充".into()),
                Content::Text("pending third input".into()),
            ],
            vec![
                Content::Text("pending image question".into()),
                png(),
                Content::Text("pending image details".into()),
            ],
            vec![png(), Content::Text("pending image first".into())],
            vec![png()],
        ]
    }

    fn session_with(messages: Vec<Message>) -> Session {
        let dir = Session::sessions_dir().unwrap();
        let mut session = Session::create(dir.parent().unwrap(), "mock", "mock-model").unwrap();
        session.append_batch(messages).unwrap();
        crate::message::validate(&session.messages).unwrap();
        session
    }

    fn compactable_session(tail: Option<Vec<Content>>) -> Session {
        let mut messages = vec![
            Message::user_text("old answered request".into()),
            Message::assistant(
                vec![Content::Text("old completed answer".into())],
                Some(Usage {
                    input: 900,
                    output: 5,
                    ..Default::default()
                }),
            ),
        ];
        if let Some(content) = tail {
            messages.push(user(content));
        }
        session_with(messages)
    }

    /// 包括主 JSONL、备份和临时文件：拒绝摘要连相同内容的 rewrite 也不能发生。
    fn files(session: &Session) -> Vec<(OsString, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(session.path.parent().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{}.", session.header.id))
            })
            .map(|entry| (entry.file_name(), std::fs::read(entry.path()).unwrap()))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    fn assert_resumable(session: &Session) {
        crate::message::validate(&session.messages).unwrap();
        let before = files(session);
        let resumed = Session::resume(&session.header.id).unwrap();
        assert_eq!(resumed.header, session.header);
        assert_eq!(resumed.messages, session.messages);
        assert_eq!(resumed.path, session.path);
        assert_eq!(files(session), before, "resume must not salvage or rewrite");
    }

    fn assert_unanswered_content(summary: &Message, tail: &[Content]) {
        let mut preserved = summary.content.clone();
        let Content::Text(first) = &mut preserved[0] else {
            panic!("summary must begin with text");
        };
        let prefix =
            format!("{SUMMARY_PREFIX}\n{SUMMARY_TEXT}\n\nLatest unanswered user message:\n");
        *first = first.strip_prefix(&prefix).unwrap().to_owned();
        if first.is_empty() {
            preserved.remove(0);
        }
        assert_eq!(
            preserved, tail,
            "every original block must survive in order"
        );
    }

    fn first_text(message: &Message) -> &str {
        let Content::Text(text) = &message.content[0] else {
            panic!("expected a leading text block");
        };
        text
    }

    fn assert_compacted(rx: &mut mpsc::Receiver<Event>) {
        assert!(matches!(
            rx.try_recv().unwrap(),
            Event::Compacted {
                before_tokens: 900,
                after_tokens: 30,
            }
        ));
        assert!(rx.try_recv().is_err(), "only one compaction event expected");
    }

    #[tokio::test]
    async fn successful_summaries_preserve_all_blocks_for_every_entry() {
        if isolated("successful_summaries_preserve_all_blocks_for_every_entry").await {
            return;
        }
        for entry in Entry::ALL {
            for tail in std::iter::once(None).chain(unanswered_inputs().into_iter().map(Some)) {
                let mut session = compactable_session(tail.clone());
                let original = std::fs::read(&session.path).unwrap();
                let provider = MockProvider::new(vec![Response::events(summary_events())]);
                let agent = agent(provider.clone());
                let (tx, mut rx) = mpsc::channel(8);
                entry.apply(&agent, &mut session, &tx).await.unwrap();
                assert_eq!(session.messages.len(), 1);
                match tail {
                    Some(tail) => assert_unanswered_content(&session.messages[0], &tail),
                    None => assert_eq!(
                        session.messages[0].content,
                        vec![Content::Text(format!("{SUMMARY_PREFIX}\n{SUMMARY_TEXT}"))]
                    ),
                }
                assert_compacted(&mut rx);
                let written = files(&session);
                assert_eq!(written.len(), 2, "one main file and one backup");
                assert!(written.iter().any(|(name, bytes)| {
                    name.to_string_lossy().ends_with(".bak.jsonl") && *bytes == original
                }));
                assert_resumable(&session);
                // 已替换的旧 usage 不应触发重复压缩。
                assert!(!maybe(&agent, &mut session, &tx).await.unwrap());
                assert_eq!(files(&session), written);
                assert!(rx.try_recv().is_err());
                let seen = provider.seen.lock().unwrap();
                assert_eq!(seen.len(), 1);
                let prompt = first_text(&seen[0][0]);
                assert!(prompt.contains("user: old answered request"));
                assert!(prompt.contains("assistant: old completed answer"));
                assert!(
                    !prompt.contains("pending "),
                    "unanswered input bypasses summarization"
                );
                assert!(!prompt.contains(PNG_B64));
            }
        }
    }

    fn rejected_responses() -> Vec<(&'static str, Response, &'static str)> {
        let mut cases = Vec::new();
        for (label, reason) in [
            ("length / MaxTokens", StopReason::MaxTokens),
            ("unknown stop reason", StopReason::Other),
            ("tool stop reason", StopReason::ToolUse),
        ] {
            cases.push((
                label,
                Response::events(vec![StreamEvent::TextDelta("partial".into()), done(reason)]),
                "expected EndTurn",
            ));
        }
        for (label, event) in [
            (
                "tool start",
                StreamEvent::ToolUseStart {
                    id: "unexpected-tool".into(),
                    name: "shell".into(),
                },
            ),
            ("tool delta", StreamEvent::ToolUseDelta("{}".into())),
            ("tool end", StreamEvent::ToolUseEnd),
        ] {
            cases.push((
                label,
                Response::events(vec![
                    StreamEvent::TextDelta("summary with tool".into()),
                    event,
                    done(StopReason::EndTurn),
                ]),
                "unexpected tool event",
            ));
        }
        cases.extend([
            (
                "empty text",
                Response::events(vec![done(StopReason::EndTurn)]),
                "empty text",
            ),
            (
                "whitespace text",
                Response::events(vec![
                    StreamEvent::TextDelta(" \n\t".into()),
                    done(StopReason::EndTurn),
                ]),
                "empty text",
            ),
            ("empty EOF", Response::events(vec![]), "without Done"),
            (
                "partial EOF",
                Response::events(vec![StreamEvent::TextDelta("partial".into())]),
                "without Done",
            ),
            (
                "connect error",
                Response::Error(ProviderError::Transport("connect failed".into())),
                "connect failed",
            ),
            (
                "stream error",
                Response::Events(vec![
                    Ok(StreamEvent::TextDelta("partial".into())),
                    Err(ProviderError::Transport("stream failed".into())),
                ]),
                "stream failed",
            ),
            (
                "error after Done",
                Response::Events(vec![
                    Ok(StreamEvent::TextDelta("looks complete".into())),
                    Ok(done(StopReason::EndTurn)),
                    Err(ProviderError::Transport("late error".into())),
                ]),
                "late error",
            ),
        ]);
        for (label, extra) in [
            ("duplicate Done", done(StopReason::EndTurn)),
            ("late MaxTokens", done(StopReason::MaxTokens)),
            ("late text", StreamEvent::TextDelta("extra".into())),
            ("late tool", StreamEvent::ToolUseEnd),
        ] {
            let mut events = summary_events();
            events.push(extra);
            cases.push((label, Response::events(events), "after Done"));
        }
        cases
    }

    #[tokio::test]
    async fn rejected_summaries_leave_history_and_files_unchanged() {
        if isolated("rejected_summaries_leave_history_and_files_unchanged").await {
            return;
        }
        for entry in Entry::ALL {
            for (label, response, diagnostic) in rejected_responses() {
                let mut session = compactable_session(Some(unanswered_inputs().remove(2)));
                // 已有备份也不能被失败的摘要覆盖或清理。
                session.rewrite(session.messages.clone()).unwrap();
                let before = files(&session);
                let memory = session.messages.clone();
                let provider = MockProvider::new(vec![response]);
                let agent = agent(provider.clone());
                let (tx, mut rx) = mpsc::channel(8);
                let err = entry.apply(&agent, &mut session, &tx).await.unwrap_err();
                assert!(
                    err.to_string().contains(diagnostic),
                    "{entry:?} / {label}: {err:#}"
                );
                assert_eq!(session.messages, memory, "{entry:?} / {label}");
                assert_eq!(files(&session), before, "{entry:?} / {label}");
                assert!(
                    rx.try_recv().is_err(),
                    "failed summaries emit no Compacted event"
                );
                assert_eq!(provider.seen.lock().unwrap().len(), 1);
                assert_resumable(&session);
            }
        }
    }

    #[tokio::test]
    async fn cancellation_is_a_noop_at_each_summary_boundary() {
        if isolated("cancellation_is_a_noop_at_each_summary_boundary").await {
            return;
        }
        for automatic in [false, true] {
            for stage in ["before", "connect", "text", "done", "eof"] {
                let token = CancellationToken::new();
                let entered = Arc::new(Notify::new());
                let responses = match stage {
                    "before" => {
                        token.cancel();
                        vec![]
                    }
                    "connect" => vec![Response::PendingConnect(entered.clone())],
                    "text" => vec![Response::PendingAfter(
                        vec![StreamEvent::TextDelta("partial".into())],
                        entered.clone(),
                    )],
                    "done" => vec![Response::PendingAfter(summary_events(), entered.clone())],
                    "eof" => vec![Response::CancelAtEof(summary_events(), token.clone())],
                    _ => unreachable!(),
                };
                let provider = MockProvider::new(responses);
                let agent = agent(provider.clone());
                let mut session = compactable_session(Some(unanswered_inputs().remove(2)));
                let before = files(&session);
                let memory = session.messages.clone();
                let (tx, mut rx) = mpsc::channel(8);
                let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                    tokio::join!(
                        async {
                            if automatic {
                                maybe_cancelable(&agent, &mut session, &tx, &token).await
                            } else {
                                force_cancelable(&agent, &mut session, &tx, &token).await
                            }
                        },
                        async {
                            if matches!(stage, "connect" | "text" | "done") {
                                entered.notified().await;
                                token.cancel();
                            }
                        }
                    )
                })
                .await
                .expect("summary cancellation must finish within the hard timeout");
                assert!(!result.unwrap(), "{automatic} / {stage}");
                assert!(token.is_cancelled());
                assert_eq!(session.messages, memory);
                assert_eq!(files(&session), before);
                assert!(
                    rx.try_recv().is_err(),
                    "cancellation emits no error or success"
                );
                assert_eq!(
                    provider.seen.lock().unwrap().len(),
                    usize::from(stage != "before")
                );
                assert_resumable(&session);
            }
        }
    }

    #[tokio::test]
    async fn no_compactable_history_never_requests_a_summary() {
        if isolated("no_compactable_history_never_requests_a_summary").await {
            return;
        }
        for entry in Entry::ALL {
            let cases = std::iter::once(vec![])
                .chain(unanswered_inputs().into_iter().map(|tail| vec![user(tail)]))
                .chain(std::iter::once(vec![Message::user_text(format!(
                    "{SUMMARY_PREFIX}\nprevious summary"
                ))]));
            for messages in cases {
                let mut session = session_with(messages);
                let before = files(&session);
                let memory = session.messages.clone();
                let provider = MockProvider::new(vec![]);
                let agent = agent(provider.clone());
                let (tx, mut rx) = mpsc::channel(8);
                entry.apply(&agent, &mut session, &tx).await.unwrap();
                assert_eq!(session.messages, memory);
                assert_eq!(files(&session), before);
                assert!(provider.seen.lock().unwrap().is_empty());
                assert!(rx.try_recv().is_err());
                assert_resumable(&session);
            }
        }
    }

    #[tokio::test]
    async fn tool_result_history_is_summarized_with_exact_call_pairing() {
        if isolated("tool_result_history_is_summarized_with_exact_call_pairing").await {
            return;
        }
        for entry in Entry::ALL {
            let mut session = session_with(vec![
                Message::user_text("read two files".into()),
                Message::assistant(
                    ["t1", "t2"]
                        .into_iter()
                        .map(|id| Content::ToolUse {
                            id: id.into(),
                            name: "read".into(),
                            input: serde_json::json!({"path": id}),
                        })
                        .collect(),
                    Some(Usage {
                        input: 900,
                        ..Default::default()
                    }),
                ),
                user(vec![
                    Content::Text("result context".into()),
                    Content::ToolResult {
                        tool_use_id: "t2".into(),
                        content: "second result".into(),
                        is_error: false,
                    },
                    png(),
                    Content::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "first result".into(),
                        is_error: false,
                    },
                ]),
            ]);
            let (head, tail) = split_head_tail(&session).unwrap();
            crate::message::validate(&head).unwrap();
            assert_eq!(head, session.messages);
            assert!(
                tail.is_none(),
                "tool results cannot become orphan summary blocks"
            );
            let provider = MockProvider::new(vec![Response::events(summary_events())]);
            let agent = agent(provider.clone());
            let (tx, mut rx) = mpsc::channel(8);
            entry.apply(&agent, &mut session, &tx).await.unwrap();
            assert_compacted(&mut rx);
            assert_eq!(session.messages.len(), 1);
            assert_eq!(session.messages[0].content.len(), 1);
            assert_resumable(&session);
            let seen = provider.seen.lock().unwrap();
            let prompt = first_text(&seen[0][0]);
            for fragment in [
                "assistant tool_use read (t1)",
                "assistant tool_use read (t2)",
                "tool_result t1 (error=false): first result",
                "tool_result t2 (error=false): second result",
                "user: result context",
                "[image: image/png,",
            ] {
                assert!(prompt.contains(fragment), "{fragment}");
            }
            assert!(!prompt.contains(PNG_B64));
        }
    }

    #[tokio::test]
    async fn resumed_run_turn_keeps_every_pending_block_and_new_task() {
        if isolated("resumed_run_turn_keeps_every_pending_block_and_new_task").await {
            return;
        }
        const NEW_TASK: &str = "latest resumed task\n最新任务原文必须保留";
        for fail_summary in [false, true] {
            for mut pending in unanswered_inputs() {
                let original = compactable_session(Some(pending.clone()));
                let mut session = Session::resume(&original.header.id).unwrap();
                assert_eq!(session.messages, original.messages);
                let mut expected = session.messages.clone();
                expected
                    .last_mut()
                    .unwrap()
                    .content
                    .push(Content::Text(NEW_TASK.into()));
                pending.push(Content::Text(NEW_TASK.into()));
                let responses = if fail_summary {
                    vec![Response::events(vec![
                        StreamEvent::TextDelta("truncated summary".into()),
                        done(StopReason::MaxTokens),
                    ])]
                } else {
                    vec![
                        Response::events(summary_events()),
                        Response::events(vec![
                            StreamEvent::TextDelta("completed latest task".into()),
                            done(StopReason::EndTurn),
                        ]),
                    ]
                };
                let provider = MockProvider::new(responses);
                let agent = agent(provider.clone());
                let (tx, mut rx) = mpsc::channel(16);
                let result = agent
                    .run_turn(&mut session, NEW_TASK.into(), CancellationToken::new(), tx)
                    .await;
                let mut events = Vec::new();
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
                if fail_summary {
                    assert!(result.unwrap_err().to_string().contains("MaxTokens"));
                    assert_eq!(
                        session.messages, expected,
                        "failure must keep the new input active"
                    );
                    assert!(events.is_empty());
                    // 唯一备份来自 prepare_input 合并新输入，没有额外摘要 rewrite。
                    assert_eq!(files(&session).len(), 2);
                } else {
                    assert_eq!(result.unwrap(), TurnResult::Done);
                    assert_eq!(session.messages.len(), 2);
                    assert_unanswered_content(&session.messages[0], &pending);
                    assert_eq!(first_text(&session.messages[1]), "completed latest task");
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| matches!(e, Event::Compacted { .. }))
                            .count(),
                        1
                    );
                }
                assert_resumable(&session);
                let seen = provider.seen.lock().unwrap();
                assert_eq!(seen.len(), if fail_summary { 1 } else { 2 });
                let summary_prompt = first_text(&seen[0][0]);
                assert!(summary_prompt.contains("old answered request"));
                assert!(!summary_prompt.contains(NEW_TASK));
                assert!(!summary_prompt.contains(PNG_B64));
                if !fail_summary {
                    assert_eq!(seen[1], session.messages[..1]);
                    assert_unanswered_content(&seen[1][0], &pending);
                    assert_eq!(
                        seen[1][0].content.last(),
                        Some(&Content::Text(NEW_TASK.into()))
                    );
                }
            }
        }
    }

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
        assert_eq!(tail, Some(vec![Content::Text("follow-up".into())]));

        // 末尾是工具结果（已回答）：全部进 head，无 tail。
        session.messages.push(Message::assistant(
            vec![Content::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            None,
        ));
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
        crate::message::validate(&head).unwrap();
        assert_eq!(head.len(), session.messages.len());
        assert_eq!(tail, None);
    }
}
