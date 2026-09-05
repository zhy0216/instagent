//! 渲染（第二版 §2.11）：文本流式直接打印；工具调用打一行
//! `▶ shell  ls -la`，完成后打预览和耗时；每轮末尾打 usage；不做 markdown。
//!
//! 输出契约（ADR 0003 D4）：**text 模式 stdout 输出模型文本流（包括中间说明）
//! （TextDelta）**，工具事件（`▶` / `✓` / `✗` 行）、预览、usage、compaction
//! 提示、错误与一切诊断统一走 **stderr**——`>file` / 管道消费方拿到纯答案。
//! text 模式 stdout 写失败（如 EPIPE）一律忽略、不改退出码；失败退出由 `main` 保证
//! 非零退出码且 stderr 末行 `error: {message}`。
//!
//! 两个流都以 `&mut dyn Write` 注入，路由规则可纯逻辑断言（T5）。

use std::io::Write;

use serde_json::Value;

use instagent::agent::Event;
use instagent::message::Usage;

/// Drain progress continuously; JSON mode obtains its answer from the session.
pub async fn print_events(mut rx: tokio::sync::mpsc::Receiver<Event>, format: super::OutputFormat) {
    let mut state = RenderState::default();
    let mut stdout = std::io::stdout();
    let mut discard = std::io::sink();
    let out: &mut dyn Write = match format {
        super::OutputFormat::Text => &mut stdout,
        super::OutputFormat::Json => &mut discard,
    };
    let mut diag = std::io::stderr();
    while let Some(event) = rx.recv().await {
        render_event(&event, &mut state, out, &mut diag);
    }
    finish_turn(&mut state, out, &mut diag);
}

/// 单轮渲染状态（打印机任务独占）。
#[derive(Default)]
pub struct RenderState {
    /// 本轮是否正在输出流式文本（决定要不要补换行）。
    pub text_open: bool,
    pub last_usage: Option<Usage>,
}

/// 渲染一条 loop 事件：`out` = 答案流（stdout），`diag` = 诊断流（stderr）。
/// 两侧的写错误都忽略（D4：EPIPE 不改退出码）。
pub fn render_event(
    event: &Event,
    state: &mut RenderState,
    out: &mut dyn Write,
    diag: &mut dyn Write,
) {
    match event {
        Event::TextDelta(delta) => {
            if !delta.is_empty() {
                state.text_open = true;
                let _ = write!(out, "{delta}");
                let _ = out.flush();
            }
        }
        Event::ToolStart { name, input, .. } => {
            close_text(state, out);
            let _ = writeln!(diag, "▶ {name}  {}", call_summary(name, input));
        }
        Event::ToolDone {
            preview,
            is_error,
            elapsed_ms,
            ..
        } => {
            let marker = if *is_error { "✗" } else { "✓" };
            let _ = writeln!(diag, "  {marker} {elapsed_ms} ms");
            for line in preview.lines() {
                let _ = writeln!(diag, "  │ {line}");
            }
        }
        Event::Usage(usage) => {
            state.last_usage = Some(*usage);
        }
        Event::Compacted {
            before_tokens,
            after_tokens,
        } => {
            close_text(state, out);
            let _ = writeln!(diag, "· compacted {before_tokens} → {after_tokens} tokens");
        }
        Event::Error(text) => {
            close_text(state, out);
            let _ = writeln!(diag, "error: {text}");
        }
    }
}

/// 一轮结束：补换行（答案流）+ usage 行（诊断流）。
pub fn finish_turn(state: &mut RenderState, out: &mut dyn Write, diag: &mut dyn Write) {
    close_text(state, out);
    if let Some(usage) = state.last_usage.take() {
        let _ = writeln!(
            diag,
            "usage: in={} out={} cache_read={} cache_write={}",
            usage.input, usage.output, usage.cache_read, usage.cache_write
        );
    }
}

/// 补答案流的收尾换行：文本块被工具事件/错误打断或整轮结束时调用。
fn close_text(state: &mut RenderState, out: &mut dyn Write) {
    if state.text_open {
        let _ = writeln!(out);
        state.text_open = false;
    }
}

/// 工具调用一行摘要：已知意图字段优先，其余压缩 JSON（≤100 字符）。
pub fn call_summary(_name: &str, input: &Value) -> String {
    if let Value::Object(map) = input {
        for key in [
            "command",
            "path",
            "file_path",
            "url",
            "pattern",
            "glob",
            "skill",
            "query",
            "text",
        ] {
            if let Some(value) = map.get(key).and_then(Value::as_str) {
                return truncate_one_line(value);
            }
        }
    }
    truncate_one_line(&input.to_string())
}

fn truncate_one_line(text: &str) -> String {
    const MAX_CHARS: usize = 100;
    let flat = text.replace('\n', " ⏎ ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(MAX_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn call_summary_prefers_intent_fields() {
        assert_eq!(
            call_summary("shell", &json!({"command": "ls -la"})),
            "ls -la"
        );
        assert_eq!(call_summary("cmd__x", &json!({"a": 1})), r#"{"a":1}"#);
        let long = "x".repeat(300);
        assert_eq!(
            call_summary("t", &json!({"text": long})).chars().count(),
            101 // 100 + 省略号
        );
    }

    #[test]
    fn usage_is_printed_once_per_turn() {
        let mut state = RenderState::default();
        let (mut out, mut diag) = (Vec::new(), Vec::new());
        render_event(
            &Event::Usage(Usage {
                input: 10,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            }),
            &mut state,
            &mut out,
            &mut diag,
        );
        render_event(
            &Event::Usage(Usage {
                input: 12,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            }),
            &mut state,
            &mut out,
            &mut diag,
        );
        assert_eq!(state.last_usage.unwrap().input, 12);
    }

    // ---- ADR 0003 D4：stdout 只放答案文本，其余全走 stderr ----

    fn rendered(events: &[Event]) -> (String, String) {
        let mut state = RenderState::default();
        let (mut out, mut diag) = (Vec::new(), Vec::new());
        for event in events {
            render_event(event, &mut state, &mut out, &mut diag);
        }
        finish_turn(&mut state, &mut out, &mut diag);
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(diag).unwrap(),
        )
    }

    #[test]
    fn text_deltas_go_to_answer_stream_only() {
        let (out, diag) = &rendered(&[
            Event::TextDelta("hello ".into()),
            Event::TextDelta("world".into()),
        ]);
        assert_eq!(out, "hello world\n", "答案流以收尾换行结束");
        assert!(diag.is_empty(), "纯文本轮诊断流必须为空: {diag:?}");
    }

    #[test]
    fn tool_events_usage_compaction_error_go_to_diag_stream() {
        let (out, diag) = &rendered(&[
            Event::ToolStart {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            },
            Event::ToolDone {
                id: "c1".into(),
                preview: "file.txt".into(),
                is_error: false,
                elapsed_ms: 7,
            },
            Event::Compacted {
                before_tokens: 9,
                after_tokens: 3,
            },
            Event::Usage(Usage {
                input: 12,
                output: 5,
                cache_read: 1,
                cache_write: 2,
            }),
            Event::Error("boom".into()),
        ]);
        assert!(out.is_empty(), "诊断事件不得污染答案流: {out:?}");
        assert!(diag.contains("▶ shell  ls"), "{diag}");
        assert!(diag.contains("✓ 7 ms"), "{diag}");
        assert!(diag.contains("│ file.txt"), "{diag}");
        assert!(diag.contains("· compacted 9 → 3 tokens"), "{diag}");
        assert!(
            diag.contains("usage: in=12 out=5 cache_read=1 cache_write=2"),
            "{diag}"
        );
        assert!(diag.contains("error: boom"), "{diag}");
    }

    #[test]
    fn text_block_is_closed_before_diag_lines() {
        let (out, diag) = &rendered(&[
            Event::TextDelta("partial".into()),
            Event::ToolStart {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({}),
            },
            Event::TextDelta("more".into()),
        ]);
        assert_eq!(out, "partial\nmore\n", "每个文本块各补一次换行");
        let tool_line = diag.lines().position(|l| l.starts_with('▶')).unwrap();
        assert!(tool_line < diag.lines().count(), "{diag}");
    }
}
