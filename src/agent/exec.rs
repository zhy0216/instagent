//! 工具执行（todo 13 / A1）：同一 assistant turn 内的 tool call 按三段式
//! 执行——① 串行决策（按序处理 malformed / 取消 / PreToolUse hook，交互点
//! 与策略钩子不进并行阶段）；② [`execution_units`] 按能力元数据划分执行
//! 单元（同单元全为 ReadOnly 且资源键互不相同，Serial 调用各自独立成单元）；
//! ③ 单元按原顺序串行执行，单元内调用以 [`PARALLEL_TOOL_LIMIT`] 有界并发。
//! 每个 call 恰产出一个 ToolResult，顺序与 calls 一致（`02` 会话不变量）。
//!
//! 能力元数据见 [`crate::tools::CallCapability`]：`read_only` 来自各来源
//! 声明，资源键由内核按已知工具语义分配。工具自动执行、无交互审批
//! （ADR 0002）；sandbox 是唯一强制隔离层（ADR 0003 D6），并行不引入新的
//! 信任假设——只并行只读调用，冲突判定失败的方向永远是降级为串行。
//! hooks 不受影响：PreToolUse 仍在串行阶段按序执行，PostToolUse 在各任务
//! 内随结果触发。
//!
//! 共享总预算与取消：并行任务共用同一 [`crate::tools::ToolCtx`] 与会话
//! 图片预算（[`SESSION_IMAGE_BUDGET`]）；取消时在途任务经 `tokio::select!`
//! 短路（子进程靠 `kill_on_drop` 回收）、未启动的单元补
//! `Content::interrupted`，事件与结果仍按调用 id 一一对应，会话不变量不变。

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::agent::event::{self, Event};
use crate::agent::hook_ctx;
use crate::agent::Agent;
use crate::agent::AssistantStream;
use crate::hooks::HookDecision;
use crate::hooks::HookEvent;
use crate::message::Content;
use crate::message::Message;
use crate::message::Role;
use crate::message::INTERRUPTED_TEXT;
use crate::session::Session;
use crate::tools::CallCapability;
use crate::tools::ImageData;
use crate::tools::ToolCall;
use crate::tools::ToolCtx;
use crate::tools::ToolKind;
use crate::tools::ToolOutput;

/// 并行执行并发上限（todo 13）：同一 turn 内最多同时执行这么多个工具调用。
/// 模型单条消息的调用数天然有限，上限只防 IO / 子进程风暴。
pub const PARALLEL_TOOL_LIMIT: usize = 4;

/// 会话图片总字节预算（todo 13 / S15 最小行为）：同一会话全部
/// `Content::Image` 解码后字节之和的上限；新图片会使总量超限时不再附上，
/// 对应工具结果带可操作提示。请求侧兜底见
/// [`crate::provider::openai::REQUEST_IMAGE_BUDGET`]；不引入 RM2 的
/// blob/reference 存储，不引入新 decoder。
pub const SESSION_IMAGE_BUDGET: u64 = 64 * 1024 * 1024;

impl Agent {
    /// 三段式执行 tool call（todo 13）：
    /// ① 串行决策——按序处理 malformed / 取消 / PreToolUse hook（语义与逐字
    ///    同串行版一致，交互点与策略钩子不进并行阶段）；
    /// ② 按能力元数据把待执行调用划成执行单元（[`execution_units`]）——
    ///    同单元全为 ReadOnly 且资源键互不相同，Serial 调用各自独立成单元；
    /// ③ 单元按原顺序串行执行，单元内调用以 [`PARALLEL_TOOL_LIMIT`] 有界并发。
    /// 每个 call 产出一个 ToolResult，顺序与 calls 一致；图片块受会话预算
    /// `image_budget` 约束（loop 使用 [`SESSION_IMAGE_BUDGET`]，测试注入小预算）。
    pub(super) async fn execute_calls(
        &self,
        session: &Session,
        calls: &[ToolCall],
        streamed: &AssistantStream,
        cancel: &CancellationToken,
        events: &mpsc::Sender<Event>,
        image_budget: u64,
    ) -> Vec<Content> {
        let ctx = ToolCtx {
            cwd: session.header.cwd.clone(),
            cancel: cancel.clone(),
        };

        // 阶段 1：串行决策。
        let mut dispositions: Vec<Disposition> = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some(detail) = streamed.malformed.get(&call.id) {
                dispositions.push(Disposition::Finished(Content::tool_result(
                    call,
                    ToolOutput::err(format!(
                        "tool input JSON is broken and the tool was not executed: {detail}"
                    )),
                )));
                continue;
            }
            if streamed.cancelled || cancel.is_cancelled() {
                dispositions.push(Disposition::Finished(Content::interrupted(call)));
                continue;
            }
            // hook 触发点：PreToolUse 在调用之前；阻止 → is_error 结果，工具不执行。
            // 等待受 token 约束：取消则 drop hook future（子进程经 kill_on_drop
            // 回收），该调用记 interrupted，后续调用走上面的取消分支。
            if let Some(hooks) = &self.hooks {
                let hook_ctx = hook_ctx(session, HookEvent::PreToolUse)
                    .with_tool(call.name.clone(), Some(call.input.clone()));
                let decision = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    decision = hooks.run(&hook_ctx) => Some(decision),
                };
                let Some(decision) = decision else {
                    dispositions.push(Disposition::Finished(Content::interrupted(call)));
                    continue;
                };
                if let HookDecision::Block(reason) = decision {
                    let text = format!("blocked by PreToolUse hook: {reason}");
                    event::emit(
                        events,
                        Event::ToolDone {
                            id: call.id.clone(),
                            preview: text.clone(),
                            is_error: true,
                            elapsed_ms: 0,
                        },
                    )
                    .await;
                    dispositions.push(Disposition::Finished(Content::tool_result(
                        call,
                        ToolOutput::err(text),
                    )));
                    continue;
                }
            }
            // 工具 inventory（`capability` 内做 `list` 枚举）等待受 token 约束：
            // 取消则 drop 等待 future，该调用记 interrupted。
            let cap = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    dispositions.push(Disposition::Finished(Content::interrupted(call)));
                    continue;
                }
                cap = self.tools.capability(call) => cap,
            };
            dispositions.push(Disposition::Ready {
                call: call.clone(),
                cap,
            });
        }

        // 阶段 2/3：单元串行、单元内并发；结果按原下标落槽。
        let mut slots: Vec<Option<Vec<Content>>> = dispositions
            .iter()
            .map(|d| match d {
                Disposition::Finished(content) => Some(vec![content.clone()]),
                Disposition::Ready { .. } => None,
            })
            .collect();
        let semaphore = Semaphore::new(PARALLEL_TOOL_LIMIT);
        let image_used = AtomicU64::new(session_image_bytes(&session.messages));
        for unit in execution_units(&dispositions) {
            if cancel.is_cancelled() {
                // 未启动的单元：补 interrupted（不变量：每个 ToolUse 恰一个答复）。
                for &idx in &unit {
                    if let Disposition::Ready { call, .. } = &dispositions[idx] {
                        slots[idx] = Some(vec![Content::interrupted(call)]);
                    }
                }
                continue;
            }
            let tasks = unit.iter().map(|&idx| {
                let Disposition::Ready { call, .. } = &dispositions[idx] else {
                    unreachable!("execution units only reference Ready dispositions");
                };
                async {
                    let _permit = semaphore
                        .acquire()
                        .await
                        .expect("semaphore is never closed");
                    event::emit(
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
                    // 图片预算（todo 13）：超限不附上，结果文本带可操作提示。
                    // 图片校验（A01）：先过消息校验核心再原子预留预算；坏图片
                    // 不消耗后续调用的额度，丢弃并转成可诊断的错误结果。
                    let mut kept_image = None;
                    if let Some(img) = output.image.take() {
                        if let Some(note) = image_validation_note(call, &img) {
                            output.text.push_str(
                                "\n\n[image omitted: tool image output failed \
                                 validation and was not attached: ",
                            );
                            output.text.push_str(&note);
                            output.text.push(']');
                            output.is_error = true;
                        } else {
                            match reserve_image_bytes(
                                &image_used,
                                img.decoded_bytes(),
                                image_budget,
                            ) {
                                Ok(()) => kept_image = Some(img),
                                Err(note) => output.text.push_str(&note),
                            }
                        }
                    }
                    event::emit(
                        events,
                        Event::ToolDone {
                            id: call.id.clone(),
                            preview: preview_text(&output.text),
                            is_error: output.is_error,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        },
                    )
                    .await;
                    // hook 触发点：PostToolUse 观察事件，决策忽略。等待受 token
                    // 约束：取消则 drop hook future（子进程经 kill_on_drop 回收），
                    // 结果照常落盘（工具已执行完）。
                    if let Some(hooks) = &self.hooks {
                        let hook_ctx = hook_ctx(session, HookEvent::PostToolUse)
                            .with_tool(call.name.clone(), Some(call.input.clone()))
                            .with_tool_output(output.text.clone());
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {}
                            _ = hooks.run(&hook_ctx) => {}
                        }
                    }
                    // 图片块与 ToolResult 进同一条 user 消息（结果在前）。
                    let mut blocks = vec![Content::tool_result(call, output)];
                    if let Some(img) = kept_image {
                        blocks.push(Content::Image(img));
                    }
                    blocks
                }
            });
            for (&idx, blocks) in unit.iter().zip(futures::future::join_all(tasks).await) {
                slots[idx] = Some(blocks);
            }
        }
        slots
            .into_iter()
            .flat_map(|slot| slot.expect("every call is answered exactly once (todo 13)"))
            .collect()
    }
}

/// 阶段 1 对单个调用的处置（todo 13）：Finished 已有结果（malformed /
/// 取消 / hook 阻止），Ready 等待阶段 2/3 执行。
enum Disposition {
    Ready { call: ToolCall, cap: CallCapability },
    Finished(Content),
}

/// 执行单元划分（todo 13）：按原顺序扫描，Ready 调用仅当 ReadOnly 且资源键
/// 与单元内其它调用互不相同时加入当前单元；Serial 调用各自独立成单元；
/// Finished 不执行、不影响划分。单元按原顺序串行执行，单元内并行——
/// 写操作、同资源冲突与依赖调用因此保持原顺序。
fn execution_units(dispositions: &[Disposition]) -> Vec<Vec<usize>> {
    let mut units: Vec<Vec<usize>> = Vec::new();
    let mut batch: Vec<usize> = Vec::new();
    let mut resources: HashSet<String> = HashSet::new();
    for (idx, disposition) in dispositions.iter().enumerate() {
        let Disposition::Ready { cap, .. } = disposition else {
            continue;
        };
        match cap.kind {
            ToolKind::Serial => {
                flush_batch(&mut units, &mut batch, &mut resources);
                units.push(vec![idx]);
            }
            ToolKind::ReadOnly => {
                if let Some(resource) = &cap.resource {
                    if resources.contains(resource) {
                        flush_batch(&mut units, &mut batch, &mut resources);
                    }
                    resources.insert(resource.clone());
                }
                batch.push(idx);
            }
        }
    }
    flush_batch(&mut units, &mut batch, &mut resources);
    units
}

fn flush_batch(
    units: &mut Vec<Vec<usize>>,
    batch: &mut Vec<usize>,
    resources: &mut HashSet<String>,
) {
    if !batch.is_empty() {
        units.push(std::mem::take(batch));
        resources.clear();
    }
}

/// 会话历史中全部图片块的解码字节合计（会话图片预算的基线）。
fn session_image_bytes(messages: &[Message]) -> u64 {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|block| match block {
            Content::Image(image) => image.decoded_bytes(),
            _ => 0,
        })
        .sum()
}

/// 工具图片进入结果前的校验（A01）：用既有消息校验核心在最小合成历史上
/// 验证该图片，复用 mime / canonical base64 / 尺寸约束，不回显图片数据。
/// 返回 `None` = 合法可附上；`Some(note)` = 脱敏诊断，调用方丢弃图片并把
/// note 写进错误 ToolResult（不让坏图片毒化会话落盘）。
fn image_validation_note(call: &ToolCall, image: &ImageData) -> Option<String> {
    let probe = vec![
        Message::user_text("probe".to_string()),
        Message::assistant(
            vec![Content::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            }],
            None,
        ),
        Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: "probe".to_string(),
                    is_error: false,
                },
                Content::Image(image.clone()),
            ],
            ts: 0,
            usage: None,
        },
    ];
    crate::message::validate(&probe)
        .err()
        .map(|err| format!("{err:#}"))
}

/// 原子地预留图片字节预算：接受则累计，超限返回可操作提示。CAS 防同 turn
/// 的并行调用同时通过检查——超限必拒，不留竞争误差。
fn reserve_image_bytes(used: &AtomicU64, bytes: u64, budget: u64) -> Result<(), String> {
    let mut current = used.load(Ordering::SeqCst);
    loop {
        if current + bytes > budget {
            return Err(image_budget_note(current, bytes, budget));
        }
        match used.compare_exchange(current, current + bytes, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

/// 被拒图片附在工具结果文本里的可操作提示（todo 13）。
fn image_budget_note(used: u64, bytes: u64, budget: u64) -> String {
    format!(
        "\n\n[image not attached: session image budget of {} MiB exceeded (session already \
         holds {used} image bytes, this image is {bytes} bytes). Finish the current analysis \
         or start a new session before reading more images.]",
        budget / (1024 * 1024)
    )
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
    use crate::message::Role;
    use crate::tools::ImageData;

    #[test]
    fn execution_units_group_independent_read_only_and_isolate_conflicts() {
        let ready = |kind: ToolKind, resource: Option<&str>| Disposition::Ready {
            call: ToolCall {
                id: "t".to_string(),
                name: "probe".to_string(),
                input: serde_json::json!({}),
            },
            cap: CallCapability {
                kind,
                resource: resource.map(str::to_string),
            },
        };
        let finished = || Disposition::Finished(Content::Text("done".to_string()));

        // 独立只读（资源键互异 / 无资源键）→ 同一单元。
        let d = vec![
            ready(ToolKind::ReadOnly, Some("f1")),
            ready(ToolKind::ReadOnly, Some("f2")),
            ready(ToolKind::ReadOnly, None),
        ];
        assert_eq!(execution_units(&d), vec![vec![0, 1, 2]]);

        // 同资源键冲突 → 按原顺序串行。
        let d = vec![
            ready(ToolKind::ReadOnly, Some("f1")),
            ready(ToolKind::ReadOnly, Some("f1")),
        ];
        assert_eq!(execution_units(&d), vec![vec![0], vec![1]]);

        // Serial（写 / 顺序敏感）独立成单元，两侧只读各自成组。
        let d = vec![
            ready(ToolKind::ReadOnly, None),
            ready(ToolKind::Serial, None),
            ready(ToolKind::ReadOnly, None),
        ];
        assert_eq!(execution_units(&d), vec![vec![0], vec![1], vec![2]]);

        // Finished（malformed / 取消 / hook 阻止）不参与划分。
        let d = vec![
            finished(),
            ready(ToolKind::ReadOnly, None),
            finished(),
            ready(ToolKind::ReadOnly, None),
        ];
        assert_eq!(execution_units(&d), vec![vec![1, 3]]);
    }

    #[test]
    fn session_image_budget_rejects_with_actionable_note() {
        let used = AtomicU64::new(0);
        reserve_image_bytes(&used, 60, 100).unwrap();
        assert_eq!(used.load(Ordering::SeqCst), 60);
        let note = reserve_image_bytes(&used, 50, 100).unwrap_err();
        assert!(note.contains("image budget"), "{note}");
        assert!(note.contains("start a new session"), "可操作提示：{note}");
        assert!(note.contains("60"), "{note}");
        assert_eq!(used.load(Ordering::SeqCst), 60, "被拒图片不消耗预算");
        reserve_image_bytes(&used, 40, 100).unwrap();
        assert_eq!(used.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn session_image_bytes_sums_history_images() {
        let messages = vec![
            Message::user_text("hi".into()),
            Message::assistant(vec![Content::Text("see".into())], None),
            Message {
                role: Role::User,
                content: vec![
                    Content::Image(ImageData {
                        data: "Zm9v".into(),
                        media_type: "image/png".into(),
                    }),
                    Content::Image(ImageData {
                        data: "Zg==".into(),
                        media_type: "image/png".into(),
                    }),
                ],
                ts: 0,
                usage: None,
            },
        ];
        assert_eq!(session_image_bytes(&messages), 4); // 3 + 1 解码字节
        assert_eq!(session_image_bytes(&messages[..2]), 0);
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
}
