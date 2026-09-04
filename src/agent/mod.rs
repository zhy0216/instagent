//! Agent loop（第二版 §2.5）：一个普通的 async 循环，不做状态机。
//!
//! `assemble` / `run_turn` / `stream_assistant`；所有退出路径统一走
//! `finish_turn` 补 ToolResult，保证 `02` 的会话不变量（§6 风险 2）；
//! 取消用 `tokio::select!`。
//! hooks 触发点（`17` 接线，§2.7）：UserPromptSubmit / PreToolUse /
//! PostToolUse / Stop 在 `run_turn` 内；SessionStart / SessionEnd 走
//! [`Agent::run_session_event`]，由 `18` 的会话生命周期调用。
//! `hooks` 为 None 时行为与未接 hooks 完全一致。
//!
//! 事件通道消费契约（todo 11 / A2）：channel 只给 UI 看，调用方自行选择
//! 消费方式——及时 drain（交互式 UI）、给足容量（近似 unbounded、事件不丢）、
//! 或不消费（事件在 [`EMIT_GRACE`] 宽限后被丢弃并计数，见
//! [`dropped_event_count`]）。loop 永不因接收端落后或断开而无限期卡住。
//!
//! 残缺 provider stream（todo 11 / A3）：`ToolUseEnd` 前 EOF 的 tool-use
//! 保留已收集片段、按 malformed 提升（loop 给它补 is_error ToolResult，
//! 模型可见），`Done` 前 EOF 记入 [`AssistantStream::incomplete`]；两者都
//! 同时以 `Event::Error` 上报事件层。流内 `ProviderError` 仍原样上抛
//! （已是结构化错误，且不能吞掉——ContextOverflow 重试依赖它）。

pub mod compact;
pub mod event;
pub mod prompt;

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::hooks::HookContext;
use crate::hooks::HookDecision;
use crate::hooks::HookEvent;
use crate::hooks::Hooks;
use crate::hooks::STOP_BLOCK_LIMIT;
use crate::message::Content;
use crate::message::Message;
use crate::message::Role;
use crate::message::Usage;
use crate::message::INTERRUPTED_TEXT;
use crate::provider::Provider;
use crate::provider::Request;
use crate::provider::StreamEvent;
use crate::session::Session;
use crate::tools::Registry;
use crate::tools::ToolCall;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::ProviderError;

pub use event::Event;

/// 事件通道背压宽限（A2）：channel 满时 `Agent::emit` 最多再等这么久，
/// 之后丢弃事件并计数——turn 不为 UI 无限期等待。
pub const EMIT_GRACE: Duration = Duration::from_millis(250);

// ponytail: 进程级计数——instagent 单 agent 进程；若将来并发多 Agent 需要
// 分开记账，再把计数挪进调用方自选的 sink。
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

/// 进程启动以来因接收端断开 / 背压超宽限而被丢弃的事件总数（A2 消费契约的
/// 计数面；drop 明细见 tracing debug）。
pub fn dropped_event_count() -> u64 {
    DROPPED_EVENTS.load(Ordering::Relaxed)
}

fn note_event_dropped(reason: &str) {
    let total = DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!("{reason}，事件丢弃（累计 {total}）");
}

/// loop 的行为参数（从 `01` Config 装配而来）。
#[derive(Debug, Clone)]
pub struct AgentCfg {
    pub model: String,
    pub max_tokens: u32,
    /// 默认 1000（goose agent.rs:85），可配。
    pub max_turns: u32,
    /// registry 四级顺序解析后的值（`10`）；`assemble` 里没有 registry，
    /// 取 `config.context_limit` 覆盖 → [`crate::provider::context_limit_for`]。
    pub context_limit: u32,
    /// 默认 0.8（第二版 §2.7）。
    pub compaction_threshold: f32,
}

pub struct Agent {
    pub cfg: AgentCfg,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<Registry>,
    /// 无 hooks 插件时为 None（`17`）。
    pub hooks: Option<Arc<Hooks>>,
    /// 系统提示注入位：每个 MCP server 的 instructions（含 server 名前缀），
    /// 由 `18` 装配时填入。
    pub mcp_instructions: Vec<String>,
    /// 系统提示注入位：每个 skill 的 `name — description`，由 `18` 装配时填入。
    pub skill_lines: Vec<String>,
}

/// `run_turn` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnResult {
    Done,
    Interrupted,
    MaxTurns,
}

/// `stream_assistant` 的产物：折叠出的 assistant 消息 + 输入 JSON 解析失败的
/// ToolUse（id → 错误文本，loop 直接补 is_error 结果、不执行）+ 流是否被取消
/// + 残缺流的结构化说明（A3）。
#[derive(Debug)]
pub struct AssistantStream {
    pub message: Message,
    pub malformed: HashMap<String, String>,
    pub cancelled: bool,
    /// provider 流在协议层被截断时的结构化 note（None = 完整）；同一 note
    /// 以 `Event::Error` 上报事件层。取消不算截断（调用方动作，已由
    /// `cancelled` 表达）。
    pub incomplete: Option<StreamIncomplete>,
}

impl AssistantStream {
    fn cancelled_empty() -> Self {
        Self {
            message: Message::assistant(Vec::new(), None),
            malformed: HashMap::new(),
            cancelled: true,
            incomplete: None,
        }
    }
}

/// 残缺 provider stream 的结构化说明（A3 / todo 11）：只记录截断阶段，
/// 不回显流原文——错误模型对齐 09 的 message 校验（阶段 + 约束计数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamIncomplete {
    /// `StreamEvent::Done` 之前流就到 EOF / 断连（已收集的块照常保留落盘）。
    pub ended_without_done: bool,
    /// 一个 tool-use 块在 `StreamEvent::ToolUseEnd` 之前流就结束：原始片段
    /// 保留为 input 是残缺字符串的 ToolUse，并记入 `malformed`，由 loop
    /// 产出 is_error ToolResult 告诉模型（不再静默丢弃）。
    pub tool_use_without_end: bool,
}

impl std::fmt::Display for StreamIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut reasons: Vec<&str> = Vec::new();
        if self.ended_without_done {
            reasons.push("stream reached EOF before Done");
        }
        if self.tool_use_without_end {
            reasons.push("1 tool-use block ended before ToolUseEnd (partial input kept, answered as malformed)");
        }
        write!(f, "provider stream truncated: {}", reasons.join("; "))
    }
}

impl Agent {
    /// config + provider + 已注册工具源的 Registry（+ hooks）装配；CLI `18` 调用。
    pub fn assemble(
        config: &Config,
        provider: Arc<dyn Provider>,
        tools: Registry,
        hooks: Option<Hooks>,
    ) -> crate::Result<Agent> {
        let model = config
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .ok_or_else(|| anyhow::anyhow!("config.model is required to assemble the agent"))?;
        let context_limit = config
            .context_limit
            .unwrap_or_else(|| crate::provider::context_limit_for(&model));
        Ok(Agent {
            cfg: AgentCfg {
                model,
                max_tokens: config.max_tokens,
                max_turns: config.max_turns,
                context_limit,
                compaction_threshold: config.compaction_threshold,
            },
            provider,
            tools: Arc::new(tools),
            hooks: hooks.map(Arc::new),
            mcp_instructions: Vec::new(),
            skill_lines: Vec::new(),
        })
    }

    /// 第二版 §2.5 伪代码：append user → 循环 {compact::maybe → stream_assistant
    /// → append assistant → 无 tool call 即 Done → 串行执行 →
    /// 结果合成一条 user 消息}。
    pub async fn run_turn(
        &self,
        session: &mut Session,
        text: String,
        cancel: CancellationToken,
        events: mpsc::Sender<Event>,
    ) -> crate::Result<TurnResult> {
        // hook 触发点（`17` §2.7）：UserPromptSubmit 不可阻止，决策忽略。
        if let Some(hooks) = &self.hooks {
            let ctx = hook_ctx(session, HookEvent::UserPromptSubmit).with_message(text.clone());
            let _ = hooks.run(&ctx).await;
        }
        session.append(Message::user_text(text))?;
        let mut overflow_retried = false;
        let mut stop_blocks = 0u32;
        for _ in 0..self.cfg.max_turns {
            if cancel.is_cancelled() {
                return Ok(TurnResult::Interrupted);
            }
            compact::maybe(self, session, &events).await?;
            let streamed = match self.stream_assistant(session, &cancel, &events).await {
                Ok(streamed) => streamed,
                Err(e) => {
                    let overflow = matches!(
                        e.downcast_ref::<ProviderError>(),
                        Some(ProviderError::ContextOverflow)
                    );
                    if overflow && !overflow_retried {
                        overflow_retried = true;
                        compact::force(self, session, &events).await?;
                        continue;
                    }
                    Self::emit(&events, Event::Error(e.to_string())).await;
                    return Err(e);
                }
            };

            let calls = streamed.message.tool_uses();
            let results = self
                .execute_calls(session, &calls, &streamed, &cancel, &events)
                .await;

            // Stop hook 载荷要带助手文本；finish_turn 会 move 掉消息，先取。
            let last_assistant = if self.hooks.is_some() {
                assistant_text(&streamed.message)
            } else {
                String::new()
            };

            // 所有退出路径统一落盘：assistant 与答复它的 user 消息一起写，
            // 保证 02 的会话不变量。
            finish_turn(session, streamed.message, results)?;

            if streamed.cancelled || cancel.is_cancelled() {
                return Ok(TurnResult::Interrupted);
            }
            if calls.is_empty() {
                // Stop 被阻止 → 注入提醒 user 消息、本轮继续跑（§2.7）。
                if self
                    .stop_hook(session, last_assistant, &mut stop_blocks)
                    .await?
                {
                    continue;
                }
                return Ok(TurnResult::Done);
            }
        }
        Ok(TurnResult::MaxTurns)
    }

    /// 逐个执行 tool call：malformed / 取消短路 → PreToolUse hook →
    /// emit ToolStart → 执行 → emit ToolDone → PostToolUse hook。
    /// 每个 call 产出一个 ToolResult，顺序与 calls 一致。
    async fn execute_calls(
        &self,
        session: &Session,
        calls: &[ToolCall],
        streamed: &AssistantStream,
        cancel: &CancellationToken,
        events: &mpsc::Sender<Event>,
    ) -> Vec<Content> {
        let ctx = ToolCtx {
            cwd: session.header.cwd.clone(),
            cancel: cancel.clone(),
        };
        let mut results: Vec<Content> = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some(detail) = streamed.malformed.get(&call.id) {
                results.push(Content::tool_result(
                    call,
                    ToolOutput::err(format!(
                        "tool input JSON is broken and the tool was not executed: {detail}"
                    )),
                ));
                continue;
            }
            if streamed.cancelled || cancel.is_cancelled() {
                results.push(Content::interrupted(call));
                continue;
            }
            // hook 触发点：PreToolUse 在调用之前；阻止 → is_error 结果，工具不执行。
            if let Some(hooks) = &self.hooks {
                let hook_ctx = hook_ctx(session, HookEvent::PreToolUse)
                    .with_tool(call.name.clone(), Some(call.input.clone()));
                if let HookDecision::Block(reason) = hooks.run(&hook_ctx).await {
                    let text = format!("blocked by PreToolUse hook: {reason}");
                    Self::emit(
                        events,
                        Event::ToolDone {
                            id: call.id.clone(),
                            preview: text.clone(),
                            is_error: true,
                            elapsed_ms: 0,
                        },
                    )
                    .await;
                    results.push(Content::tool_result(call, ToolOutput::err(text)));
                    continue;
                }
            }
            Self::emit(
                events,
                Event::ToolStart {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                },
            )
            .await;
            let started = Instant::now();
            let mut output = tokio::select! {
                biased;
                _ = cancel.cancelled() => ToolOutput::err(INTERRUPTED_TEXT.to_string()),
                out = self.tools.call(call, &ctx) => out,
            };
            Self::emit(
                events,
                Event::ToolDone {
                    id: call.id.clone(),
                    preview: preview_text(&output.text),
                    is_error: output.is_error,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
            )
            .await;
            // hook 触发点：PostToolUse 观察事件，决策忽略。
            if let Some(hooks) = &self.hooks {
                let hook_ctx = hook_ctx(session, HookEvent::PostToolUse)
                    .with_tool(call.name.clone(), Some(call.input.clone()))
                    .with_tool_output(output.text.clone());
                let _ = hooks.run(&hook_ctx).await;
            }
            // 图片块与 ToolResult 进同一条 user 消息（结果在前）。
            let image = output.image.take();
            results.push(Content::tool_result(call, output));
            if let Some(img) = image {
                results.push(Content::Image(img));
            }
        }
        results
    }

    /// hook 触发点：Stop。被阻止 → 注入提醒 user 消息并返回 true（本轮继续跑）；
    /// 无 hooks / 未阻止 / 连续阻止超上限（默认 8）强制结束返回 false，防死循环（§2.7）。
    async fn stop_hook(
        &self,
        session: &mut Session,
        last_assistant: String,
        stop_blocks: &mut u32,
    ) -> crate::Result<bool> {
        let Some(hooks) = &self.hooks else {
            return Ok(false);
        };
        let ctx = hook_ctx(session, HookEvent::Stop).with_message(last_assistant);
        if let HookDecision::Block(reason) = hooks.run(&ctx).await {
            *stop_blocks += 1;
            if *stop_blocks > STOP_BLOCK_LIMIT {
                tracing::warn!(
                    "Stop hook 连续阻止 {stop_blocks} 次（上限 {STOP_BLOCK_LIMIT}），强制结束本轮"
                );
                return Ok(false);
            }
            session.append(Message::user_text(format!(
                "Stop hook blocked ending this turn:\n\n{reason}\n\n\
                 Address this policy hook denial before trying to stop again."
            )))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// hook 触发点入口：SessionStart / SessionEnd 由 `18` 的会话生命周期
    /// 调用（不可阻止事件，调用方忽略决策即可）。无 hooks 时直接 Allow。
    pub async fn run_session_event(
        &self,
        event: HookEvent,
        session: &Session,
    ) -> crate::Result<HookDecision> {
        let Some(hooks) = &self.hooks else {
            return Ok(HookDecision::Allow);
        };
        Ok(hooks.run(&hook_ctx(session, event)).await)
    }

    /// StreamEvent 折叠成一条 assistant Message，同时转发 TextDelta；
    /// JSON 损坏记入 `malformed`（loop 生成 is_error 的 ToolResult 告诉模型）。
    /// ContextOverflow 原样上抛，由 run_turn 触发强制压缩重试。
    /// 取消时返回已折叠的部分消息并置 `cancelled`。
    pub async fn stream_assistant(
        &self,
        session: &Session,
        cancel: &CancellationToken,
        events: &mpsc::Sender<Event>,
    ) -> crate::Result<AssistantStream> {
        let specs = self.tools.list().await;
        let ctx = prompt::PromptContext {
            tools: &specs,
            cwd: &session.header.cwd,
            now: chrono::Utc::now(),
            mcp_instructions: &self.mcp_instructions,
            skill_lines: &self.skill_lines,
        };
        let system = prompt::system(&ctx);
        let request = Request {
            model: &self.cfg.model,
            system: &system,
            messages: &session.messages,
            tools: &specs,
            max_tokens: self.cfg.max_tokens,
            temperature: None, // 待有配置来源再打开
        };

        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(AssistantStream::cancelled_empty());
            }
            stream = self.provider.stream(request) => stream?,
        };

        let mut blocks: Vec<Content> = Vec::new();
        let mut text = String::new();
        let mut pending: Option<(String, String, String)> = None;
        let mut malformed = HashMap::new();
        let mut usage: Option<Usage> = None;
        let mut cancelled = false;
        loop {
            let item = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                item = stream.next() => item,
            };
            let Some(item) = item else { break };
            match item? {
                StreamEvent::TextDelta(delta) => {
                    if !delta.is_empty() {
                        Self::emit(events, Event::TextDelta(delta.clone())).await;
                        text.push_str(&delta);
                    }
                }
                StreamEvent::ToolUseStart { id, name } => {
                    flush_text(&mut blocks, &mut text);
                    pending = Some((id, name, String::new()));
                }
                StreamEvent::ToolUseDelta(delta) => {
                    if let Some((_, _, buf)) = pending.as_mut() {
                        buf.push_str(&delta);
                    }
                }
                StreamEvent::ToolUseEnd => {
                    let Some((id, name, buf)) = pending.take() else {
                        continue;
                    };
                    match serde_json::from_str::<Value>(&buf) {
                        Ok(input) => blocks.push(Content::ToolUse { id, name, input }),
                        Err(e) => {
                            malformed.insert(id.clone(), e.to_string());
                            blocks.push(Content::ToolUse {
                                id,
                                name,
                                input: Value::String(buf),
                            });
                        }
                    }
                }
                StreamEvent::Done { usage: u, .. } => {
                    usage = Some(u);
                    Self::emit(events, Event::Usage(u)).await;
                }
            }
        }
        // 残缺 stream（A3）：取消是调用方动作（cancelled 已表达），pending
        // 直接丢；EOF / 断连则保留片段并产出结构化 note。
        let mut incomplete: Option<StreamIncomplete> = None;
        if cancelled {
            drop(pending);
        } else {
            if let Some((id, name, buf)) = pending.take() {
                malformed.insert(
                    id.clone(),
                    "stream ended before ToolUseEnd (partial arguments kept)".to_string(),
                );
                blocks.push(Content::ToolUse {
                    id,
                    name,
                    input: Value::String(buf),
                });
                incomplete
                    .get_or_insert_with(StreamIncomplete::default)
                    .tool_use_without_end = true;
            }
            if usage.is_none() {
                incomplete
                    .get_or_insert_with(StreamIncomplete::default)
                    .ended_without_done = true;
            }
        }
        flush_text(&mut blocks, &mut text);
        if let Some(truncated) = incomplete {
            // API 层由字段承载，事件层统一发 Event::Error（渲染侧可见）。
            Self::emit(events, Event::Error(truncated.to_string())).await;
        }
        Ok(AssistantStream {
            message: Message::assistant(blocks, usage),
            malformed,
            cancelled,
            incomplete,
        })
    }

    /// 事件通道只给 UI 看（消费契约见模块头）：接收端断开不该弄死 loop；
    /// channel 满最多等 [`EMIT_GRACE`]，之后丢弃并计数
    /// （[`dropped_event_count`]），turn 永不因背压无限期卡住。
    pub(crate) async fn emit(events: &mpsc::Sender<Event>, event: Event) {
        let pending = match events.try_send(event) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                note_event_dropped("event 接收端已断开");
                return;
            }
            Err(mpsc::error::TrySendError::Full(event)) => event,
        };
        match tokio::time::timeout(EMIT_GRACE, events.send(pending)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => note_event_dropped("event 接收端在发送途中断开"),
            Err(_) => note_event_dropped("event 通道在背压宽限后仍然满载"),
        }
    }
}

/// hook 上下文公共基底：事件 + session id + working dir（`17` §2.7），
/// 各触发点再链式 `.with_*` 补专属字段。
fn hook_ctx(session: &Session, event: HookEvent) -> HookContext {
    HookContext::new(event, session.header.id.clone()).with_working_dir(session.header.cwd.clone())
}

/// assistant 消息的全部文本拼接（Stop hook 的 last message）。
fn assistant_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn flush_text(blocks: &mut Vec<Content>, text: &mut String) {
    if !text.is_empty() {
        blocks.push(Content::Text(std::mem::take(text)));
    }
}

/// assistant + 工具结果一次性落盘（空 content 跳过，保 validate 不变量 3）。
fn finish_turn(
    session: &mut Session,
    assistant: Message,
    results: Vec<Content>,
) -> crate::Result<()> {
    if !assistant.content.is_empty() {
        session.append(assistant)?;
    }
    if !results.is_empty() {
        session.append(Message {
            role: Role::User,
            content: results,
            ts: chrono::Utc::now().timestamp(),
            usage: None,
        })?;
    }
    Ok(())
}

/// ToolDone 预览：前 10 行 / 1KB（渲染细则归 `18`）。
fn preview_text(text: &str) -> String {
    const MAX_LINES: usize = 10;
    const MAX_BYTES: usize = 1024;
    let lines: Vec<&str> = text.lines().collect();
    let mut truncated = lines.len() > MAX_LINES;
    let mut preview = lines[..lines.len().min(MAX_LINES)].join("\n");
    if preview.len() > MAX_BYTES {
        let mut cut = MAX_BYTES;
        while !preview.is_char_boundary(cut) {
            cut -= 1;
        }
        preview.truncate(cut);
        truncated = true;
    }
    if truncated {
        preview.push_str("\n[output truncated]");
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compact::COMPACTION_PROMPT;
    use crate::message::SUMMARY_PREFIX;
    use crate::session::SessionHeader;
    use crate::tools::BuiltinTools;
    use crate::tools::ImageData;
    use crate::tools::ToolSource;
    use crate::tools::ToolSpec;
    use async_trait::async_trait;
    use futures::stream;
    use futures::stream::BoxStream;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Debug)]
    enum Scripted {
        Ok {
            events: Vec<StreamEvent>,
            cancel_at_end: Option<CancellationToken>,
        },
        Err(ProviderError),
    }

    fn done(input: u32, output: u32) -> StreamEvent {
        StreamEvent::Done {
            usage: Usage {
                input,
                output,
                ..Default::default()
            },
            stop_reason: crate::provider::StopReason::EndTurn,
        }
    }

    fn tool_use_shell(id: &str, raw_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseStart {
                id: id.into(),
                name: "shell".into(),
            },
            StreamEvent::ToolUseDelta(raw_json.into()),
            StreamEvent::ToolUseEnd,
        ]
    }

    fn scripted(events: Vec<StreamEvent>) -> Scripted {
        Scripted::Ok {
            events,
            cancel_at_end: None,
        }
    }

    /// 流耗尽后把 cancel 置位再收尾：模拟"流中途取消"。
    fn scripted_then_cancel(events: Vec<StreamEvent>, token: &CancellationToken) -> Scripted {
        Scripted::Ok {
            events,
            cancel_at_end: Some(token.clone()),
        }
    }

    struct MockProvider {
        script: tokio::sync::Mutex<VecDeque<Scripted>>,
        calls: AtomicUsize,
        seen: tokio::sync::Mutex<Vec<Vec<Message>>>,
        seen_tool_counts: tokio::sync::Mutex<Vec<usize>>,
    }

    impl MockProvider {
        fn new(script: Vec<Scripted>) -> Arc<Self> {
            Arc::new(Self {
                script: tokio::sync::Mutex::new(script.into()),
                calls: AtomicUsize::new(0),
                seen: tokio::sync::Mutex::new(Vec::new()),
                seen_tool_counts: tokio::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        async fn seen(&self) -> Vec<Vec<Message>> {
            self.seen.lock().await.clone()
        }

        async fn seen_tool_counts(&self) -> Vec<usize> {
            self.seen_tool_counts.lock().await.clone()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn stream(
            &self,
            req: Request<'_>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().await.push(req.messages.to_vec());
            self.seen_tool_counts.lock().await.push(req.tools.len());
            let Some(step) = self.script.lock().await.pop_front() else {
                return Err(ProviderError::Transport("mock script exhausted".into()));
            };
            match step {
                Scripted::Err(e) => Err(e),
                Scripted::Ok {
                    events,
                    cancel_at_end,
                } => {
                    let head = stream::iter(events.into_iter().map(Ok)).boxed();
                    let tail: BoxStream<'static, Result<StreamEvent, ProviderError>> =
                        match cancel_at_end {
                            Some(token) => stream::once(async move {
                                token.cancel();
                                Ok(StreamEvent::TextDelta(String::new()))
                            })
                            .boxed(),
                            None => stream::empty().boxed(),
                        };
                    Ok(head.chain(tail).boxed())
                }
            }
        }
    }

    fn agent(provider: Arc<MockProvider>, context_limit: u32) -> Agent {
        let mut registry = Registry::new();
        registry.register(Arc::new(BuiltinTools::new(None)));
        Agent {
            cfg: AgentCfg {
                model: "mock-model".into(),
                max_tokens: 1024,
                max_turns: 10,
                context_limit,
                compaction_threshold: 0.8,
            },
            provider,
            tools: Arc::new(registry),
            hooks: None,
            mcp_instructions: Vec::new(),
            skill_lines: Vec::new(),
        }
    }

    /// 手工构造会话文件（不碰 INSTAGENT_DATA_DIR，避免与其他模块并行测试
    /// 的环境变量互踩）。
    fn temp_session(dir: &Path) -> Session {
        let header = SessionHeader {
            id: "test".into(),
            created: 0,
            cwd: dir.to_path_buf(),
            provider: "mock".into(),
            model: "mock-model".into(),
        };
        let path = dir.join("test.jsonl");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&header).unwrap()),
        )
        .unwrap();
        Session {
            header,
            messages: Vec::new(),
            path,
        }
    }

    async fn run(
        agent: &Agent,
        session: &mut Session,
        text: &str,
    ) -> (crate::Result<TurnResult>, Vec<Event>) {
        run_with(agent, session, text, CancellationToken::new()).await
    }

    async fn run_with(
        agent: &Agent,
        session: &mut Session,
        text: &str,
        cancel: CancellationToken,
    ) -> (crate::Result<TurnResult>, Vec<Event>) {
        let (tx, mut rx) = mpsc::channel(8192);
        let result = agent.run_turn(session, text.to_string(), cancel, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    fn first_text(message: &Message) -> String {
        match &message.content[0] {
            Content::Text(text) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn tool_results(message: &Message) -> Vec<(String, String, bool)> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.clone(), content.clone(), *is_error)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn full_loop_text_tool_text() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![
                StreamEvent::TextDelta("working".into()),
                StreamEvent::TextDelta("…".into()),
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "shell".into(),
                },
                StreamEvent::ToolUseDelta("{\"command\": \"ech".into()),
                StreamEvent::ToolUseDelta("o instagent-hi\"}".into()),
                StreamEvent::ToolUseEnd,
                done(10, 5),
            ]),
            scripted(vec![StreamEvent::TextDelta("all done".into()), done(20, 3)]),
        ]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "say hi").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 2);

        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(first_text(&session.messages[0]), "say hi");
        assert_eq!(
            session.messages[1].content[0],
            Content::Text("working…".into())
        );
        let calls = session.messages[1].tool_uses();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["command"], "echo instagent-hi");
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 1);
        assert!(!results[0].2, "{}", results[0].1);
        assert!(results[0].1.contains("instagent-hi"), "{}", results[0].1);
        assert_eq!(first_text(&session.messages[3]), "all done");
        assert_eq!(session.messages[3].usage.unwrap().input, 20);

        // 会话文件同步落盘（header + 4 条）；请求带内置 6 工具。
        let raw = std::fs::read_to_string(&session.path).unwrap();
        assert_eq!(raw.lines().count(), 5);
        assert_eq!(provider.seen_tool_counts().await, vec![6, 6]);

        assert!(events
            .iter()
            .any(|e| matches!(e, Event::ToolStart { name, .. } if name == "shell")));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolDone {
                is_error: false,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(e, Event::Usage(_))));
    }

    /// 产图 stub 来源（不依赖 02 的 read_image）：look_image 返回带 image 的 ToolOutput。
    const STUB_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    struct ImageSource;

    #[async_trait]
    impl ToolSource for ImageSource {
        fn id(&self) -> &str {
            "test:image"
        }

        async fn list(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "look_image".into(),
                description: "stub image producer".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
            }]
        }

        async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput {
                text: "Loaded image from stub".into(),
                is_error: false,
                image: Some(ImageData {
                    data: STUB_PNG_B64.into(),
                    media_type: "image/png".into(),
                }),
            }
        }
    }

    #[tokio::test]
    async fn tool_image_lands_in_user_message_and_survives_resume() {
        // create / resume 走 INSTAGENT_DATA_DIR（与全 crate 共用 lock_env），
        // 两个同步块各自短暂持锁，不跨 await。
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "look_image".into(),
                },
                StreamEvent::ToolUseDelta("{}".into()),
                StreamEvent::ToolUseEnd,
                done(5, 1),
            ]),
            scripted(vec![StreamEvent::TextDelta("saw it".into()), done(9, 1)]),
        ]);
        let mut test_agent = agent(provider.clone(), 100_000);
        let mut registry = Registry::new();
        registry.register(Arc::new(BuiltinTools::new(None)));
        registry.register(Arc::new(ImageSource));
        test_agent.tools = Arc::new(registry);
        let mut session = {
            let _guard = crate::config::lock_env();
            std::env::set_var("INSTAGENT_DATA_DIR", dir.path());
            Session::create(dir.path(), "mock", "mock-model").unwrap()
        };

        let (result, _) = run(&test_agent, &mut session, "look at it").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();

        // 落盘的 user 消息恰为 [ToolResult, Image]（结果在前）。
        let results = &session.messages[2].content;
        assert_eq!(results.len(), 2);
        match &results[..] {
            [Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            }, Content::Image(img)] => {
                assert_eq!(tool_use_id, "t1");
                assert!(!is_error);
                assert_eq!(content, "Loaded image from stub");
                assert_eq!(img.media_type, "image/png");
                assert_eq!(img.data, STUB_PNG_B64);
            }
            other => panic!("expected [ToolResult, Image], got {other:?}"),
        }

        // resume 后 Image 块字节不变（借 01 的 serde 形状）。
        let image_json = serde_json::to_string(&session.messages[2].content[1]).unwrap();
        assert_eq!(
            image_json,
            format!(r#"{{"data":"{STUB_PNG_B64}","media_type":"image/png"}}"#)
        );
        let resumed = {
            let _guard = crate::config::lock_env();
            std::env::set_var("INSTAGENT_DATA_DIR", dir.path());
            let resumed = Session::resume(&session.header.id).unwrap();
            std::env::remove_var("INSTAGENT_DATA_DIR");
            resumed
        };
        assert_eq!(resumed.messages, session.messages);
        assert_eq!(
            serde_json::to_string(&resumed.messages[2].content[1]).unwrap(),
            image_json
        );
    }

    #[tokio::test]
    async fn cancel_mid_stream_keeps_session_invariants() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let mut events = vec![StreamEvent::TextDelta("partial".into())];
        events.extend(tool_use_shell("t1", "{\"command\": \"echo hi\"}"));
        let provider = MockProvider::new(vec![scripted_then_cancel(events, &token)]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, _events) = run_with(&agent, &mut session, "go", token).await;
        assert_eq!(result.unwrap(), TurnResult::Interrupted);

        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[1].tool_uses().len(), 1);
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "t1");
        assert_eq!(results[0].1, INTERRUPTED_TEXT);
        assert!(results[0].2);
    }

    #[tokio::test]
    async fn cancel_before_turn_starts_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let provider = MockProvider::new(vec![]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, _events) = run_with(&agent, &mut session, "go", token).await;
        assert_eq!(result.unwrap(), TurnResult::Interrupted);
        assert_eq!(provider.calls(), 0);
        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 1);
    }

    #[tokio::test]
    async fn malformed_tool_json_gets_error_result_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "shell".into(),
                },
                StreamEvent::ToolUseDelta("{not json".into()),
                StreamEvent::ToolUseEnd,
                done(3, 1),
            ]),
            scripted(vec![StreamEvent::TextDelta("sorry".into()), done(6, 1)]),
        ]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "bad args").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert!(results[0].2);
        assert!(results[0].1.contains("JSON"), "{}", results[0].1);
        assert!(
            !events.iter().any(|e| matches!(e, Event::ToolStart { .. })),
            "malformed 调用不该执行"
        );
    }

    #[tokio::test]
    async fn large_usage_triggers_compaction_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![StreamEvent::TextDelta("a1".into()), done(900, 5)]),
            scripted(vec![
                StreamEvent::TextDelta("## summary\nwe talked".into()),
                done(100, 30),
            ]),
            scripted(vec![StreamEvent::TextDelta("a2".into()), done(50, 2)]),
        ]);
        let agent = agent(provider.clone(), 1000);
        let mut session = temp_session(dir.path());

        let (result, _) = run(&agent, &mut session, "first question").await;
        assert_eq!(result.unwrap(), TurnResult::Done);

        let (result, events) = run(&agent, &mut session, "second question").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 3);

        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 2);
        let summary = first_text(&session.messages[0]);
        assert!(summary.starts_with(SUMMARY_PREFIX));
        assert!(summary.contains("we talked"));
        assert!(summary.contains("second question"), "{summary}");
        assert_eq!(first_text(&session.messages[1]), "a2");

        let compacted = events
            .iter()
            .find(|e| matches!(e, Event::Compacted { .. }))
            .expect("Compacted event");
        match compacted {
            Event::Compacted {
                before_tokens,
                after_tokens,
            } => {
                assert_eq!(*before_tokens, 900);
                assert_eq!(*after_tokens, 30);
            }
            _ => unreachable!(),
        }

        // 摘要请求带上了历史文本，且压缩后的重试请求只剩 summary 一条。
        let seen = provider.seen().await;
        assert_eq!(seen[1].len(), 1);
        let request_text = first_text(&seen[1][0]);
        assert!(request_text.starts_with("## Task Context"));
        assert!(request_text.contains("**Conversation History:**"));
        assert!(request_text.contains("Write the summary as a Markdown document"));
        assert!(request_text.contains("first question"));
        assert!(request_text.contains("assistant: a1"));
        assert_eq!(seen[2].len(), 1);
    }

    #[tokio::test]
    async fn context_overflow_forces_compaction_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![StreamEvent::TextDelta("a1".into()), done(10, 5)]),
            Scripted::Err(ProviderError::ContextOverflow),
            scripted(vec![
                StreamEvent::TextDelta("overflowed summary".into()),
                done(700, 25),
            ]),
            scripted(vec![StreamEvent::TextDelta("a2".into()), done(60, 2)]),
        ]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, _) = run(&agent, &mut session, "first").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        let (result, events) = run(&agent, &mut session, "second").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 4, "overflow → 摘要 → 重试，共 4 次");

        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 2);
        let summary = first_text(&session.messages[0]);
        assert!(summary.contains("overflowed summary"));
        assert!(summary.contains("second"), "{summary}");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Compacted {
                before_tokens: 10,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn second_context_overflow_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Scripted::Err(ProviderError::ContextOverflow),
            Scripted::Err(ProviderError::ContextOverflow),
        ]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "hi").await;
        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::ContextOverflow)
        ));
        assert_eq!(provider.calls(), 2, "只重试一次");
        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert!(events.iter().any(|e| matches!(e, Event::Error(_))));
    }

    #[tokio::test]
    async fn compaction_prompt_placeholder_is_replaced() {
        // COMPACTION_PROMPT 的占位符被替换（防 typo：常量和 replace 同名）。
        assert!(COMPACTION_PROMPT.contains(crate::agent::compact::MESSAGES_PLACEHOLDER));
    }

    #[test]
    fn preview_cuts_long_output() {
        let long = "line\n".repeat(50);
        let preview = preview_text(&long);
        assert_eq!(preview.lines().count(), 11); // 10 行 + 截断标记
        assert!(preview.ends_with("[output truncated]"));

        let big = "a".repeat(5000);
        let preview = preview_text(&big);
        assert!(preview.len() <= 1024 + "\n[output truncated]".len());

        assert_eq!(preview_text("short"), "short");
    }

    #[tokio::test]
    async fn assemble_wires_config_into_agent() {
        let config = Config {
            model: Some("claude-sonnet-4-5".into()),
            max_tokens: 4096,
            max_turns: 7,
            context_limit: None,
            compaction_threshold: 0.5,
            ..Config::default()
        };
        let provider = MockProvider::new(vec![]);
        let agent = Agent::assemble(&config, provider, Registry::new(), None).unwrap();
        assert_eq!(agent.cfg.model, "claude-sonnet-4-5");
        assert_eq!(agent.cfg.max_turns, 7);
        assert_eq!(agent.cfg.context_limit, 200 * 1024);
        assert_eq!(agent.cfg.compaction_threshold, 0.5);
    }

    #[test]
    fn assemble_requires_a_model() {
        let provider = MockProvider::new(vec![]);
        let err = Agent::assemble(&Config::default(), provider, Registry::new(), None)
            .err()
            .expect("missing model must bail");
        assert!(err.to_string().contains("config.model"), "{err}");
    }

    // ---- hooks 触发点（`17`） ----

    /// 造一个 hooks 插件：每个事件一个独立脚本（走 ${PLUGIN_ROOT} 展开）。
    fn hook_fixture(dir: &Path, event_scripts: &[(&str, &str)]) -> Hooks {
        use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
        use crate::plugin::Plugin;
        use crate::plugin::PluginSet;
        use crate::plugin::PluginSource;
        use crate::plugin::NAMESPACE;
        use std::os::unix::fs::PermissionsExt;

        let root = dir.join("hookplug");
        std::fs::create_dir_all(root.join(NAMESPACE)).unwrap();
        std::fs::write(
            root.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"hookplug","version":"1.0.0"}}"#),
        )
        .unwrap();
        let mut groups = String::new();
        for (index, (event, body)) in event_scripts.iter().enumerate() {
            let script = root.join(format!("hook-{index}.sh"));
            std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            if !groups.is_empty() {
                groups.push(',');
            }
            groups.push_str(&format!(
                r#""{event}":[{{"hooks":[{{"command":"${{PLUGIN_ROOT}}/hook-{index}.sh"}}]}}]"#
            ));
        }
        std::fs::write(
            root.join(NAMESPACE).join("hooks.json"),
            format!(r#"{{"hooks":{{{groups}}}}}"#),
        )
        .unwrap();

        let mut set = PluginSet::default();
        set.plugins.push(Plugin {
            manifest: crate::plugin::manifest::read_manifest(&root).unwrap(),
            root,
            source: PluginSource::Extra,
        });
        Hooks::load(&set).unwrap()
    }

    #[tokio::test]
    async fn stop_hook_block_nudges_until_cap_then_forces_done() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = hook_fixture(dir.path(), &[("Stop", "echo keep going >&2\nexit 2")]);
        let steps: Vec<Scripted> = (0..=STOP_BLOCK_LIMIT)
            .map(|i| scripted(vec![StreamEvent::TextDelta(format!("a{i}")), done(1, 1)]))
            .collect();
        let provider = MockProvider::new(steps);
        let mut agent = agent(provider.clone(), 100_000);
        agent.hooks = Some(Arc::new(hooks));
        let mut session = temp_session(dir.path());

        let (result, _) = run(&agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        // 前 8 次阻止各注入一条 nudge（cap=8），第 9 次超上限强制结束。
        assert_eq!(provider.calls() as u32, STOP_BLOCK_LIMIT + 1);
        crate::message::validate(&session.messages).unwrap();
        assert_eq!(
            session.messages.len(),
            1 + 2 * STOP_BLOCK_LIMIT as usize + 1
        );
        assert_eq!(first_text(&session.messages[2]), "Stop hook blocked ending this turn:\n\n[hookplug] keep going\n\nAddress this policy hook denial before trying to stop again.");
        assert_eq!(first_text(&session.messages[17]), "a8");
    }

    #[tokio::test]
    async fn stop_hook_allow_ends_turn_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = hook_fixture(dir.path(), &[("Stop", "true")]);
        let provider = MockProvider::new(vec![scripted(vec![
            StreamEvent::TextDelta("finished".into()),
            done(1, 1),
        ])]);
        let mut agent = agent(provider.clone(), 100_000);
        agent.hooks = Some(Arc::new(hooks));
        let mut session = temp_session(dir.path());

        let (result, _) = run(&agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 1);
        assert_eq!(session.messages.len(), 2);
    }

    #[tokio::test]
    async fn pre_tool_use_block_is_error_result_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = hook_fixture(dir.path(), &[("PreToolUse", "echo no sudo >&2\nexit 2")]);
        let provider = MockProvider::new(vec![
            scripted(tool_use_shell("t1", "{\"command\": \"echo hi\"}")),
            scripted(vec![StreamEvent::TextDelta("ok".into()), done(1, 1)]),
        ]);
        let mut agent = agent(provider.clone(), 100_000);
        agent.hooks = Some(Arc::new(hooks));
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "run it").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert!(results[0].2, "阻止必须是 is_error");
        assert!(
            results[0]
                .1
                .contains("blocked by PreToolUse hook: [hookplug] no sudo"),
            "{}",
            results[0].1
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::ToolStart { .. })),
            "被阻止的工具不该执行"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::ToolDone { is_error: true, .. })));
    }

    #[tokio::test]
    async fn prompt_and_post_tool_events_observe_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hookplug").join("payloads");
        std::fs::create_dir_all(&out).unwrap();
        // UserPromptSubmit + PostToolUse 各把 stdin 载荷落到独立文件。
        let hooks = hook_fixture(
            dir.path(),
            &[
                (
                    "UserPromptSubmit",
                    "cat > \"$PLUGIN_ROOT/payloads/prompt.json\"",
                ),
                ("PostToolUse", "cat >> \"$PLUGIN_ROOT/payloads/post.json\""),
            ],
        );
        let provider = MockProvider::new(vec![
            scripted(tool_use_shell("t1", "{\"command\": \"echo hi\"}")),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(1, 1)]),
        ]);
        let mut agent = agent(provider.clone(), 100_000);
        agent.hooks = Some(Arc::new(hooks));
        let mut session = temp_session(dir.path());

        let (result, _) = run(&agent, &mut session, "hello hooks").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 4);

        let prompt: Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("prompt.json")).unwrap())
                .unwrap();
        assert_eq!(prompt["event"], "UserPromptSubmit");
        assert_eq!(prompt["message"], "hello hooks");
        assert_eq!(prompt["session_id"], "test");
        let post: Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("post.json")).unwrap()).unwrap();
        assert_eq!(post["event"], "PostToolUse");
        assert_eq!(post["tool_name"], "shell");
        assert_eq!(post["tool_input"]["command"], "echo hi");
        assert!(post["tool_output"].as_str().unwrap().contains("hi"));
    }

    #[tokio::test]
    async fn session_lifecycle_entry_runs_and_cannot_block() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let hooks = hook_fixture(
            dir.path(),
            &[(
                "SessionStart",
                &format!("touch {}\nexit 2", marker.display()),
            )],
        );
        let provider = MockProvider::new(vec![]);
        let mut hooked = agent(provider, 100_000);
        hooked.hooks = Some(Arc::new(hooks));
        let session = temp_session(dir.path());

        // 不可阻止事件：exit 2 也只观察，返回 Allow。
        let decision = hooked
            .run_session_event(HookEvent::SessionStart, &session)
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Allow);
        assert!(marker.exists(), "SessionStart 脚本应被执行");

        // 无 hooks 的 Agent：入口直接 Allow、不报错。
        let plain = agent(MockProvider::new(vec![]), 100_000);
        assert_eq!(
            plain
                .run_session_event(HookEvent::SessionEnd, &session)
                .await
                .unwrap(),
            HookDecision::Allow
        );
    }

    // ---- 事件通道消费契约（todo 11 / A2） ----

    /// 不消费 bounded channel 的调用：grace 超时后丢弃并计数，turn 照常
    /// 结束，会话不受影响（容量 1 + 2 次超宽限丢弃 ≈ 2 × EMIT_GRACE）。
    #[tokio::test]
    async fn unconsumed_channel_drops_events_and_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![
            StreamEvent::TextDelta("a".into()),
            StreamEvent::TextDelta("b".into()),
            done(1, 1),
        ])]);
        let agent = agent(provider, 100_000);
        let mut session = temp_session(dir.path());
        let (tx, _rx) = mpsc::channel(1);

        let before = dropped_event_count();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            agent.run_turn(&mut session, "go".into(), CancellationToken::new(), tx),
        )
        .await
        .expect("turn 不得因未消费的 channel 无限期卡住")
        .unwrap();
        assert_eq!(result, TurnResult::Done);
        // 容量 1：首条 TextDelta 入队，第二 delta 与 Usage 各丢一次。
        let dropped = dropped_event_count() - before;
        assert!(dropped >= 2, "至少应有 2 次丢弃计数，got {dropped}");
        crate::message::validate(&session.messages).unwrap();
        assert_eq!(session.messages.len(), 2);
    }

    #[tokio::test]
    async fn closed_receiver_counts_drops() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![
            StreamEvent::TextDelta("a".into()),
            done(1, 1),
        ])]);
        let agent = agent(provider, 100_000);
        let mut session = temp_session(dir.path());
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let before = dropped_event_count();
        let result = agent
            .run_turn(&mut session, "go".into(), CancellationToken::new(), tx)
            .await
            .unwrap();
        assert_eq!(result, TurnResult::Done);
        assert!(dropped_event_count() - before >= 2);
        crate::message::validate(&session.messages).unwrap();
    }

    // ---- 残缺 provider stream（todo 11 / A3） ----

    #[tokio::test]
    async fn stream_assistant_reports_truncation_on_api_layer() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![
            StreamEvent::ToolUseStart {
                id: "t1".into(),
                name: "shell".into(),
            },
            StreamEvent::ToolUseDelta("{\"command\": \"ec".into()),
        ])]);
        let agent = agent(provider, 100_000);
        let session = temp_session(dir.path());
        let (tx, mut rx) = mpsc::channel(8);

        let streamed = agent
            .stream_assistant(&session, &CancellationToken::new(), &tx)
            .await
            .unwrap();
        assert_eq!(
            streamed.incomplete,
            Some(StreamIncomplete {
                ended_without_done: true,
                tool_use_without_end: true,
            })
        );
        // 片段保留：ToolUse 块还在，input 是残缺原始字符串；同时记入 malformed。
        assert_eq!(streamed.message.content.len(), 1);
        assert!(
            matches!(&streamed.message.content[0], Content::ToolUse { id, name, input }
                if id == "t1" && name == "shell" && input == "{\"command\": \"ec"),
            "{:?}",
            streamed.message.content
        );
        assert!(streamed.malformed.contains_key("t1"));
        // 事件层：同一个 note 以 Event::Error 出现，文案稳定（不回显流原文之外的内容）。
        let note = match rx.recv().await.expect("note event") {
            Event::Error(text) => text,
            other => panic!("expected Error, got {other:?}"),
        };
        assert_eq!(
            note,
            "provider stream truncated: stream reached EOF before Done; \
             1 tool-use block ended before ToolUseEnd (partial input kept, answered as malformed)"
        );
    }

    /// 截断流不静默丢失：残缺 tool call 按 malformed 走 loop，模型收到
    /// is_error ToolResult，下一轮照常进行。
    #[tokio::test]
    async fn truncated_tool_stream_gets_error_result_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![
                StreamEvent::TextDelta("half".into()),
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "shell".into(),
                },
                StreamEvent::ToolUseDelta("{\"command\": \"ec".into()),
            ]),
            scripted(vec![StreamEvent::TextDelta("recovered".into()), done(2, 1)]),
        ]);
        let agent = agent(provider.clone(), 100_000);
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 2);
        crate::message::validate(&session.messages).unwrap();

        // assistant 保留文本 + 残缺 ToolUse 片段（字符串 input）。
        let calls = session.messages[1].tool_uses();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input, serde_json::json!("{\"command\": \"ec"));
        let results = tool_results(&session.messages[2]);
        assert!(results[0].2, "截断的 tool call 必须回 is_error");
        assert!(
            results[0].1.contains("stream ended before ToolUseEnd"),
            "{}",
            results[0].1
        );
        // 事件层可观察：截断 note 以 Event::Error 上报。
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Error(text) if text.starts_with("provider stream truncated:")
        )));
        assert_eq!(first_text(&session.messages[3]), "recovered");
    }

    /// Done 前 EOF（没有残缺 tool call）：也要有 note；文本片段照常保留。
    #[tokio::test]
    async fn eof_without_done_still_notes_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![StreamEvent::TextDelta(
            "cut off".to_string(),
        )])]);
        let agent = agent(provider, 100_000);
        let session = temp_session(dir.path());
        let (tx, mut rx) = mpsc::channel(8);

        let streamed = agent
            .stream_assistant(&session, &CancellationToken::new(), &tx)
            .await
            .unwrap();
        assert_eq!(
            streamed.incomplete,
            Some(StreamIncomplete {
                ended_without_done: true,
                tool_use_without_end: false,
            })
        );
        assert_eq!(first_text(&streamed.message), "cut off");
        let note = loop {
            match rx.recv().await.expect("events delivered") {
                Event::Error(text) => break text,
                _ => continue,
            }
        };
        assert_eq!(
            note,
            StreamIncomplete {
                ended_without_done: true,
                tool_use_without_end: false
            }
            .to_string()
        );
    }

    /// 取消不是截断：cancelled 路径不产出 incomplete note（既有语义不变）。
    #[tokio::test]
    async fn cancelled_stream_is_not_reported_as_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let provider = MockProvider::new(vec![scripted_then_cancel(
            vec![
                StreamEvent::TextDelta("half".into()),
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "shell".into(),
                },
                StreamEvent::ToolUseDelta("{\"comm".into()),
            ],
            &token,
        )]);
        let agent = agent(provider, 100_000);
        let session = temp_session(dir.path());
        let (tx, _rx) = mpsc::channel(8);

        let streamed = agent.stream_assistant(&session, &token, &tx).await.unwrap();
        assert!(streamed.cancelled);
        assert_eq!(streamed.incomplete, None);
        assert!(streamed.malformed.is_empty());
    }
}
