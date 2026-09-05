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
//! 职责划分（todo 19 / E10）：本文件只含 loop 本体（`run_turn` 及其退出
//! 路径）与 provider stream 折叠（`stream_assistant`）。工具执行（三段式
//! 并行、图片预算、取消回收，契约见私有模块 `exec`）、事件契约与背压
//! sink（[`event`]）、自动压缩（[`compact`]）、系统提示（[`prompt`]）
//! 各自独立成单元。[`PARALLEL_TOOL_LIMIT`]、[`SESSION_IMAGE_BUDGET`]、
//! [`EMIT_GRACE`]、[`dropped_event_count`] 经 re-export 保持原公开路径。
//!
//! 残缺 provider stream（todo 11 / A3）：`ToolUseEnd` 前 EOF 的 tool-use
//! 保留已收集片段、按 malformed 提升（loop 给它补 is_error ToolResult，
//! 模型可见），`Done` 前 EOF 记入 [`AssistantStream::incomplete`]；两者都
//! 同时以 `Event::Error` 上报事件层。流内 `ProviderError` 仍原样上抛
//! （已是结构化错误，且不能吞掉——ContextOverflow 重试依赖它）。

pub mod compact;
pub mod event;
pub mod prompt;

mod exec;

use std::collections::HashMap;
use std::sync::Arc;

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
use crate::provider::Provider;
use crate::provider::Request;
use crate::provider::StreamEvent;
use crate::session::Session;
use crate::tools::Registry;
use crate::ProviderError;

pub use event::dropped_event_count;
pub use event::Event;
pub use event::EMIT_GRACE;
pub use exec::PARALLEL_TOOL_LIMIT;
pub use exec::SESSION_IMAGE_BUDGET;

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
    /// → append assistant → 无 tool call 即 Done → 执行工具（`execute_calls`，
    /// 独立只读并行）→ 结果合成一条 user 消息}。
    pub async fn run_turn(
        &self,
        session: &mut Session,
        text: String,
        cancel: CancellationToken,
        events: mpsc::Sender<Event>,
    ) -> crate::Result<TurnResult> {
        // T2：预取消在任何 hook 或写入前直接退出（零写入、零 hook）。
        if cancel.is_cancelled() {
            return Ok(TurnResult::Interrupted);
        }
        // hook 触发点（`17` §2.7）：UserPromptSubmit 不可阻止，决策忽略。
        // T3：等待受 token 约束，取消则 drop hook future（子进程经
        // kill_on_drop 回收），零写入直接返回。
        if let Some(hooks) = &self.hooks {
            let ctx = hook_ctx(session, HookEvent::UserPromptSubmit).with_message(text.clone());
            let ran = tokio::select! {
                biased;
                _ = cancel.cancelled() => false,
                _ = hooks.run(&ctx) => true,
            };
            if !ran {
                return Ok(TurnResult::Interrupted);
            }
        }
        // T2：末尾 assistant 正常 append；末尾 user 把新文本并入原 content
        // 后原子 rewrite（保留摘要/旧输入/ToolResult/Image 顺序）。
        prepare_input(session, text)?;
        let mut overflow_retried = false;
        let mut stop_blocks = 0u32;
        for _ in 0..self.cfg.max_turns {
            if cancel.is_cancelled() {
                return Ok(TurnResult::Interrupted);
            }
            // T3：条件压缩等待可取消；取消时不改会话，由下一次检查收尾。
            compact::maybe_cancelable(self, session, &events, &cancel).await?;
            if cancel.is_cancelled() {
                return Ok(TurnResult::Interrupted);
            }
            let streamed = match self.stream_assistant(session, &cancel, &events).await {
                Ok(streamed) => streamed,
                Err(e) => {
                    let overflow = matches!(
                        e.downcast_ref::<ProviderError>(),
                        Some(ProviderError::ContextOverflow)
                    );
                    if overflow && !overflow_retried {
                        overflow_retried = true;
                        compact::force_cancelable(self, session, &events, &cancel).await?;
                        if cancel.is_cancelled() {
                            return Ok(TurnResult::Interrupted);
                        }
                        continue;
                    }
                    event::emit(&events, Event::Error(e.to_string())).await;
                    return Err(e);
                }
            };

            // T1：在 PreToolUse / ToolStart / execute_calls 之前，用既有消息
            // 校验核心检查 assistant 与历史的关系（重复/历史复用 ID、空
            // ID/name、错误块）。非法 → 零副作用：不跑 hook、不执行工具、
            // 不落盘，直接错出（历史保持可校验，下一次 run_turn 可合并继续）。
            // 空 content 照旧跳过（finish_turn 不落盘）；合法但 JSON 参数损坏
            // 的 malformed 块能通过校验，走已有 ToolResult 流程。
            if !streamed.message.content.is_empty() {
                if let Err(err) = validate_assistant(session, &streamed.message) {
                    event::emit(&events, Event::Error(err.to_string())).await;
                    return Err(err);
                }
            }

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

            // 所有退出路径统一落盘：assistant 与答复它的 user 消息批量提交，
            // 保证 02 的会话不变量。
            finish_turn(session, streamed.message, results)?;

            if streamed.cancelled || cancel.is_cancelled() {
                return Ok(TurnResult::Interrupted);
            }
            if calls.is_empty() {
                // Stop 被阻止 → 注入提醒 user 消息、本轮继续跑（§2.7）。
                if self
                    .stop_hook(session, last_assistant, &mut stop_blocks, &cancel)
                    .await?
                {
                    continue;
                }
                // T3：Stop 等待中被取消 → 按 Interrupted 收尾，不按 Done 结束。
                if cancel.is_cancelled() {
                    return Ok(TurnResult::Interrupted);
                }
                return Ok(TurnResult::Done);
            }
        }
        Ok(TurnResult::MaxTurns)
    }

    /// hook 触发点：Stop。被阻止 → 注入提醒 user 消息并返回 true（本轮继续跑）；
    /// 无 hooks / 未阻止 / 连续阻止超上限（默认 8）强制结束返回 false，防死循环（§2.7）。
    /// 等待受 token 约束：取消则 drop hook future（子进程经 kill_on_drop
    /// 回收），不注入提醒、不做决策，返回 false；调用方随后检查 token 并按
    /// Interrupted 收尾。
    async fn stop_hook(
        &self,
        session: &mut Session,
        last_assistant: String,
        stop_blocks: &mut u32,
        cancel: &CancellationToken,
    ) -> crate::Result<bool> {
        let Some(hooks) = &self.hooks else {
            return Ok(false);
        };
        let ctx = hook_ctx(session, HookEvent::Stop).with_message(last_assistant);
        let decision = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(false),
            decision = hooks.run(&ctx) => decision,
        };
        if let HookDecision::Block(reason) = decision {
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
        // T3：工具 inventory 枚举等待受 token 约束；取消则 drop 等待 future
        // 并按取消的空折叠返回（调用方按 Interrupted 收尾，不发请求）。
        let specs = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(AssistantStream::cancelled_empty());
            }
            specs = self.tools.list() => specs,
        };
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
                        event::emit(events, Event::TextDelta(delta.clone())).await;
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
                    event::emit(events, Event::Usage(u)).await;
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
            event::emit(events, Event::Error(truncated.to_string())).await;
        }
        Ok(AssistantStream {
            message: Message::assistant(blocks, usage),
            malformed,
            cancelled,
            incomplete,
        })
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

/// 新输入落盘（T2 / A02）：历史末尾为 assistant（或空历史）时正常追加一条
/// 新 user 消息；末尾为 user（摘要、未回答输入、tool results、中断/nudge 尾）
/// 时把新文本作为新 Text 块并入原 content，经原子 rewrite 落盘。
/// 原 content 块顺序不变（摘要、旧 prompt、ToolResult、Image 都在前，新文本在
/// 后），不放宽角色交替、不丢旧输入、不发明模型回答。下一次 run_turn 可直接
/// 继续，`validate` / `resume` 一致。
fn prepare_input(session: &mut Session, text: String) -> crate::Result<()> {
    let trailing_user = matches!(
        session.messages.last(),
        Some(Message {
            role: Role::User,
            ..
        })
    );
    if !trailing_user {
        session.append(Message::user_text(text))?;
        return Ok(());
    }
    let mut messages = session.messages.clone();
    if let Some(last) = messages.last_mut() {
        last.content.push(Content::Text(text));
    }
    session.rewrite(messages)
}

/// 副作用前校验（T1 / A01）：候选历史 = 现有消息 + 新收到的 assistant，用
/// `validate_for_append` 同一校验核心检查（重复/历史复用 ID、空 ID/name、错
/// 位块、交替）。调用方保证非空 content；通过后 execute_calls 才允许产生
/// PreToolUse / ToolStart / 工具副作用。
fn validate_assistant(session: &Session, assistant: &Message) -> crate::Result<()> {
    let mut candidate = session.messages.clone();
    candidate.push(assistant.clone());
    crate::message::validate_for_append(&candidate).map_err(|err| {
        anyhow::anyhow!(
            "assistant message failed validation against session history \
             (tools were not executed): {err:#}"
        )
    })
}

/// assistant + 工具结果批量落盘（02 的 `append_batch`：先预序列化+校验再写，
/// 失败零落盘、内存回滚）。空 content 跳过，保 validate 不变量 3。
fn finish_turn(
    session: &mut Session,
    assistant: Message,
    results: Vec<Content>,
) -> crate::Result<()> {
    let mut batch = Vec::with_capacity(2);
    if !assistant.content.is_empty() {
        batch.push(assistant);
    }
    if !results.is_empty() {
        batch.push(Message {
            role: Role::User,
            content: results,
            ts: chrono::Utc::now().timestamp(),
            usage: None,
        });
    }
    session.append_batch(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compact::COMPACTION_PROMPT;
    use crate::message::INTERRUPTED_TEXT;
    use crate::message::SUMMARY_PREFIX;
    use crate::session::SessionHeader;
    use crate::tools::BuiltinTools;
    use crate::tools::ImageData;
    use crate::tools::ToolCtx;
    use crate::tools::ToolOutput;
    use crate::tools::ToolSource;
    use crate::tools::ToolSpec;
    use async_trait::async_trait;
    use futures::stream;
    use futures::stream::BoxStream;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

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
        // T2：预取消在 hook/写文件前直接退出——连 user 输入都不落盘。
        assert!(session.messages.is_empty());
        let raw = std::fs::read_to_string(&session.path).unwrap();
        assert_eq!(raw.lines().count(), 1, "只有 header 行");
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

    // ---- 并行工具执行（todo 13） ----

    fn tool_use_events(id: &str, name: &str, raw_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseStart {
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolUseDelta(raw_json.into()),
            StreamEvent::ToolUseEnd,
        ]
    }

    fn agent_with(provider: Arc<MockProvider>, source: Arc<dyn ToolSource>) -> Agent {
        let mut registry = Registry::new();
        registry.register(Arc::new(BuiltinTools::new(None)));
        registry.register(source);
        let mut test_agent = agent(provider, 100_000);
        test_agent.tools = Arc::new(registry);
        test_agent
    }

    /// 只读探针：记录实时 / 峰值并发度并驻留固定时长。
    struct ConcurrencyProbe {
        active: AtomicUsize,
        max_active: AtomicUsize,
        hold: Duration,
    }

    impl ConcurrencyProbe {
        fn new(hold: Duration) -> Arc<Self> {
            Arc::new(Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                hold,
            })
        }
    }

    #[async_trait]
    impl ToolSource for ConcurrencyProbe {
        fn id(&self) -> &str {
            "test:concurrency"
        }

        async fn list(&self) -> Vec<ToolSpec> {
            vec![
                ToolSpec {
                    name: "probe_read".into(),
                    description: "read-only probe".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: true,
                },
                ToolSpec {
                    name: "probe_write".into(),
                    description: "serial probe".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: false,
                },
            ]
        }

        async fn call(&self, name: &str, input: Value, _ctx: &ToolCtx) -> ToolOutput {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.hold).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            ToolOutput::ok(format!("{name}:{}", input["tag"]))
        }
    }

    /// 两个调用互相等待：串行执行会死锁，只有并行能双双通过（原方案 §拆解）。
    struct PeerProbe {
        barrier: tokio::sync::Barrier,
    }

    #[async_trait]
    impl ToolSource for PeerProbe {
        fn id(&self) -> &str {
            "test:barrier"
        }

        async fn list(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "needs_peer".into(),
                description: "waits for a peer call".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
            }]
        }

        async fn call(&self, _name: &str, input: Value, _ctx: &ToolCtx) -> ToolOutput {
            self.barrier.wait().await;
            ToolOutput::ok(format!("met {}", input["tag"].as_str().unwrap_or_default()))
        }
    }

    /// 永不自行返回的只读调用：只有取消能回收任务（future 被 drop）。
    struct HangProbe;

    #[async_trait]
    impl ToolSource for HangProbe {
        fn id(&self) -> &str {
            "test:hang"
        }

        async fn list(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "hang".into(),
                description: "never returns".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
            }]
        }

        async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            std::future::pending::<()>().await;
            unreachable!("hang probe is only reclaimed by cancellation")
        }
    }

    /// tag=bad 时失败的只读探针：验证并行失败隔离。
    struct FlakyProbe;

    #[async_trait]
    impl ToolSource for FlakyProbe {
        fn id(&self) -> &str {
            "test:flaky"
        }

        async fn list(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "flaky".into(),
                description: "fails on tag=bad".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
            }]
        }

        async fn call(&self, _name: &str, input: Value, _ctx: &ToolCtx) -> ToolOutput {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if input["tag"] == "bad" {
                ToolOutput::err("boom".to_string())
            } else {
                ToolOutput::ok(format!("ok {}", input["tag"]))
            }
        }
    }

    #[tokio::test]
    async fn two_independent_read_only_calls_run_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = tool_use_events("t1", "needs_peer", r#"{"tag":"a"}"#);
        events.extend(tool_use_events("t2", "needs_peer", r#"{"tag":"b"}"#));
        events.push(done(4, 2));
        let provider = MockProvider::new(vec![
            scripted(events),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(
            provider.clone(),
            Arc::new(PeerProbe {
                barrier: tokio::sync::Barrier::new(2),
            }),
        );
        let mut session = temp_session(dir.path());

        let (result, _) = tokio::time::timeout(
            Duration::from_secs(10),
            run(&test_agent, &mut session, "go"),
        )
        .await
        .expect("串行执行会死锁在 barrier：独立只读调用必须并行");
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "t1");
        assert_eq!(results[1].0, "t2");
        assert!(results[0].1.contains("met a"), "{}", results[0].1);
        assert!(results[1].1.contains("met b"), "{}", results[1].1);
    }

    #[tokio::test]
    async fn parallel_calls_are_bounded_by_concurrency_limit() {
        let dir = tempfile::tempdir().unwrap();
        let probe = ConcurrencyProbe::new(Duration::from_millis(50));
        let total = PARALLEL_TOOL_LIMIT + 2;
        let mut events = Vec::new();
        for i in 1..=total {
            events.extend(tool_use_events(
                &format!("t{i}"),
                "probe_read",
                &format!(r#"{{"tag":"x{i}"}}"#),
            ));
        }
        events.push(done(6, 3));
        let provider = MockProvider::new(vec![
            scripted(events),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(provider.clone(), probe.clone());
        let mut session = temp_session(dir.path());

        let (result, _) = run(&test_agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(
            probe.max_active.load(Ordering::SeqCst),
            PARALLEL_TOOL_LIMIT,
            "峰值并发必须恰为上限（{total} 个只读调用，上限 {}）",
            PARALLEL_TOOL_LIMIT
        );
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), total);
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.0, format!("t{}", i + 1), "结果顺序与调用顺序一致");
            assert!(!r.2, "{}", r.1);
            assert!(r.1.contains(&format!("x{}", i + 1)), "{}", r.1);
        }
    }

    #[tokio::test]
    async fn serial_kind_calls_never_overlap_with_reads() {
        let dir = tempfile::tempdir().unwrap();
        let probe = ConcurrencyProbe::new(Duration::from_millis(30));
        let mut events = tool_use_events("t1", "probe_write", r#"{"tag":"w"}"#);
        events.extend(tool_use_events("t2", "probe_read", r#"{"tag":"r"}"#));
        events.push(done(4, 2));
        let provider = MockProvider::new(vec![
            scripted(events),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(provider.clone(), probe.clone());
        let mut session = temp_session(dir.path());

        let (result, _) = run(&test_agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(
            probe.max_active.load(Ordering::SeqCst),
            1,
            "写（Serial）与读分属不同单元，永不重叠"
        );
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert_eq!(results[0].0, "t1");
        assert_eq!(results[1].0, "t2", "即使串行也保持原调用顺序");
    }

    #[tokio::test]
    async fn cancel_reclaims_parallel_calls_and_keeps_invariants() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let mut events = Vec::new();
        for i in 1..=3 {
            events.extend(tool_use_events(&format!("t{i}"), "hang", "{}"));
        }
        events.push(done(3, 1));
        let provider = MockProvider::new(vec![scripted(events)]);
        let test_agent = agent_with(provider.clone(), Arc::new(HangProbe));
        let mut session = temp_session(dir.path());

        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let (result, _) = tokio::time::timeout(
            Duration::from_secs(10),
            run_with(&test_agent, &mut session, "go", token),
        )
        .await
        .expect("取消必须在有界时间内回收全部并行任务");
        assert_eq!(result.unwrap(), TurnResult::Interrupted);
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 3, "每个调用恰有一个答复");
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.0, format!("t{}", i + 1));
            assert_eq!(r.1, INTERRUPTED_TEXT);
            assert!(r.2);
        }
    }

    #[tokio::test]
    async fn parallel_failure_is_isolated_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = tool_use_events("t1", "flaky", r#"{"tag":"a"}"#);
        events.extend(tool_use_events("t2", "flaky", r#"{"tag":"bad"}"#));
        events.extend(tool_use_events("t3", "flaky", r#"{"tag":"c"}"#));
        events.push(done(6, 3));
        let provider = MockProvider::new(vec![
            scripted(events),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(provider.clone(), Arc::new(FlakyProbe));
        let mut session = temp_session(dir.path());

        let (result, _) = run(&test_agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done, "单个失败不终止 turn");
        crate::message::validate(&session.messages).unwrap();
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 3);
        assert!(!results[0].2, "{}", results[0].1);
        assert!(results[1].2, "失败必须落在 t2");
        assert_eq!(results[1].1, "boom");
        assert!(!results[2].2, "{}", results[2].1);
    }

    #[tokio::test]
    async fn events_and_results_pair_one_to_one_with_call_ids() {
        let dir = tempfile::tempdir().unwrap();
        let probe = ConcurrencyProbe::new(Duration::from_millis(20));
        let mut events_in = Vec::new();
        for i in 1..=3 {
            events_in.extend(tool_use_events(
                &format!("t{i}"),
                "probe_read",
                &format!(r#"{{"tag":"x{i}"}}"#),
            ));
        }
        events_in.push(done(6, 3));
        let provider = MockProvider::new(vec![
            scripted(events_in),
            scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(provider, probe);
        let mut session = temp_session(dir.path());

        let (result, events) = run(&test_agent, &mut session, "go").await;
        assert_eq!(result.unwrap(), TurnResult::Done);

        // 每个调用 id 恰一个 ToolStart / ToolDone，且 Start 先于 Done。
        for id in ["t1", "t2", "t3"] {
            let position = |predicate: &dyn Fn(&Event) -> bool| {
                events
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| predicate(e).then_some(i))
                    .collect::<Vec<_>>()
            };
            let starts = position(&|e| matches!(e, Event::ToolStart { id: eid, .. } if eid == id));
            let dones = position(&|e| matches!(e, Event::ToolDone { id: eid, .. } if eid == id));
            assert_eq!(starts.len(), 1, "{id} 的 ToolStart 必须恰一个");
            assert_eq!(dones.len(), 1, "{id} 的 ToolDone 必须恰一个");
            assert!(starts[0] < dones[0], "{id} 的 Start 必须先于 Done");
        }

        // tool result 与调用 id 一一对应（顺序同 calls）。
        let results = tool_results(&session.messages[2]);
        let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"]);
        crate::message::validate(&session.messages).unwrap();
    }

    // ---- 会话图片预算（todo 13 / S15 最小行为） ----

    /// 超过会话预算的单张图片：不附上，工具结果带可操作提示，turn 照常完成。
    #[tokio::test]
    async fn over_budget_image_is_rejected_with_actionable_note() {
        struct BigImage;

        #[async_trait]
        impl ToolSource for BigImage {
            fn id(&self) -> &str {
                "test:big"
            }

            async fn list(&self) -> Vec<ToolSpec> {
                vec![ToolSpec {
                    name: "big_image".into(),
                    description: "produces one huge image".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: true,
                }]
            }

            async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
                // 无 padding 全 'A' base64：解码字节 = len/4*3 > SESSION_IMAGE_BUDGET。
                let len = (SESSION_IMAGE_BUDGET as usize / 3 + 1) * 4;
                ToolOutput {
                    text: "loaded".to_string(),
                    is_error: false,
                    image: Some(ImageData {
                        data: "A".repeat(len),
                        media_type: "image/png".into(),
                    }),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            scripted(vec![
                StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "big_image".into(),
                },
                StreamEvent::ToolUseDelta("{}".into()),
                StreamEvent::ToolUseEnd,
                done(3, 1),
            ]),
            scripted(vec![StreamEvent::TextDelta("ok".into()), done(2, 1)]),
        ]);
        let test_agent = agent_with(provider.clone(), Arc::new(BigImage));
        let mut session = temp_session(dir.path());

        let (result, _) = run(&test_agent, &mut session, "look").await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        crate::message::validate(&session.messages).unwrap();
        let user = &session.messages[2];
        assert_eq!(user.content.len(), 1, "超预算图片不得附上");
        match &user.content[0] {
            Content::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error, "工具本身成功，只是图片被拒");
                assert!(
                    content.contains("[image not attached: session image budget"),
                    "{content}"
                );
                assert!(content.contains("start a new session"), "{content}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
}
