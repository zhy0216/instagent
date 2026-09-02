//! Agent loop（第二版 §2.5）：一个普通的 async 循环，不做状态机。
//!
//! TODO(16)：填 `assemble` / `run_turn` / `stream_assistant`；所有退出路径
//! 统一补 ToolResult 保证会话不变量（§6 风险 2）；取消用 `tokio::select!`。
//! hooks 触发点（SessionStart/Stop 等）由 `17` 在本文件接线，对 `agent/mod.rs`
//! 的修改仅限触发点。

pub mod approval;
pub mod compact;
pub mod event;
pub mod prompt;

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::Mode;
use crate::hooks::Hooks;
use crate::message::Message;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Registry;

pub use approval::Approval;
pub use approval::Confirm;
pub use approval::Decision;
pub use event::Event;

/// loop 的行为参数（从 `01` Config 装配而来）。
#[derive(Debug, Clone)]
pub struct AgentCfg {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub mode: Mode,
    /// 默认 1000（goose agent.rs:85），可配。
    pub max_turns: u32,
    /// registry 四级顺序解析后的值（`10`）。
    pub context_limit: u32,
    /// 默认 0.8（第二版 §2.7）。
    pub compaction_threshold: f32,
}

pub struct Agent {
    pub cfg: AgentCfg,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<Registry>,
    pub approval: Approval,
    /// 无 hooks 插件时为 None（`17`）。
    pub hooks: Option<Arc<Hooks>>,
}

/// `run_turn` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnResult {
    Done,
    Interrupted,
    MaxTurns,
}

impl Agent {
    /// config + provider + 已注册工具源的 Registry（+ hooks）装配；CLI `18` 调用。
    /// TODO(16)
    pub fn assemble(
        _config: &Config,
        _provider: Arc<dyn Provider>,
        _tools: Registry,
        _hooks: Option<Hooks>,
    ) -> crate::Result<Agent> {
        todo!("TODO(16)")
    }

    /// 第二版 §2.5 伪代码：append user → 循环 {compact::maybe → stream_assistant
    /// → append assistant → 无 tool call 即 Done → 逐个审批 + 串行执行 →
    /// 结果合成一条 user 消息}。chat 模式请求不带 tools。TODO(16)
    pub async fn run_turn(
        &self,
        _session: &mut Session,
        _text: String,
        _cancel: CancellationToken,
        _events: mpsc::Sender<Event>,
    ) -> crate::Result<TurnResult> {
        todo!("TODO(16)")
    }

    /// StreamEvent 折叠成一条 assistant Message，同时转发 TextDelta；
    /// JSON 损坏时给 is_error 的 ToolResult 告诉模型。ContextOverflow 原样上抛
    /// 由 run_turn 触发强制压缩重试。TODO(16)
    pub async fn stream_assistant(
        &self,
        _session: &Session,
        _cancel: &CancellationToken,
        _events: &mpsc::Sender<Event>,
    ) -> crate::Result<Message> {
        todo!("TODO(16)")
    }
}
