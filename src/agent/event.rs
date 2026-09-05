//! loop → 调用方的单向进度事件与通道消费契约。
//!
//! channel 供调用方观察进度，调用方自行选择消费方式——及时 drain、
//! 给足容量（近似 unbounded、事件不丢）、或不消费（事件在 [`EMIT_GRACE`]
//! 宽限后被丢弃并计数，见 [`dropped_event_count`]）。loop 永不因接收端
//! 落后或断开而无限期卡住。

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

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

/// 事件通道背压宽限（A2）：channel 满时 `emit` 最多再等这么久，
/// 之后丢弃事件并计数——turn 不为调用方无限期等待。
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

/// 事件通道供调用方观察进度（消费契约见模块头）：接收端断开不该弄死 loop；
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
