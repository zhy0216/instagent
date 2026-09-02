//! 渲染与审批提示（第二版 §2.11）：文本流式直接打印；工具调用打一行
//! `▶ shell  ls -la`，完成后打预览和耗时；每轮末尾打 usage；不做 markdown。
//! 审批提示 `allow this call? [y]es / [a]lways / [n]o: ` 接 `16` 的 Confirm。

use std::io::Write;

use async_trait::async_trait;
use serde_json::Value;

use instagent::agent::Confirm;
use instagent::agent::Decision;
use instagent::agent::Event;
use instagent::message::Usage;
use instagent::tools::ToolCall;

/// 单轮渲染状态（打印机任务独占）。
#[derive(Default)]
pub struct RenderState {
    /// 本轮是否正在输出流式文本（决定要不要补换行）。
    pub text_open: bool,
    pub last_usage: Option<Usage>,
}

/// 渲染一条 loop 事件。
pub fn render_event(event: &Event, state: &mut RenderState) {
    match event {
        Event::TextDelta(delta) => {
            if !delta.is_empty() {
                state.text_open = true;
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
        }
        Event::ToolStart { name, input, .. } => {
            close_text(state);
            println!("▶ {name}  {}", call_summary(name, input));
        }
        Event::ToolDone {
            preview,
            is_error,
            elapsed_ms,
            ..
        } => {
            let marker = if *is_error { "✗" } else { "✓" };
            println!("  {marker} {elapsed_ms} ms");
            for line in preview.lines() {
                println!("  │ {line}");
            }
        }
        Event::Usage(usage) => {
            state.last_usage = Some(*usage);
        }
        Event::Compacted {
            before_tokens,
            after_tokens,
        } => {
            close_text(state);
            println!("· compacted {before_tokens} → {after_tokens} tokens");
        }
        Event::Error(text) => {
            close_text(state);
            println!("error: {text}");
        }
    }
}

/// 一轮结束：补换行 + usage 行。
pub fn finish_turn(state: &mut RenderState) {
    close_text(state);
    if let Some(usage) = state.last_usage.take() {
        println!(
            "usage: in={} out={} cache_read={} cache_write={}",
            usage.input, usage.output, usage.cache_read, usage.cache_write
        );
    }
}

fn close_text(state: &mut RenderState) {
    if state.text_open {
        println!();
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

/// CLI 审批回调：阻塞读 stdin（v1 唯一实现；loop 走 `16` 的 Confirm trait）。
pub struct CliConfirm;

#[async_trait]
impl Confirm for CliConfirm {
    async fn confirm(&self, call: &ToolCall) -> Decision {
        println!("  {}", call_summary(&call.name, &call.input));
        print!("allow this call? [y]es / [a]lways / [n]o: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Decision::Deny("no answer (EOF)".to_string());
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Decision::Allow,
            "a" | "always" => Decision::AllowAlways,
            other => Decision::Deny(format!("user answered {other:?}")),
        }
    }
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
        render_event(
            &Event::Usage(Usage {
                input: 10,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            }),
            &mut state,
        );
        render_event(
            &Event::Usage(Usage {
                input: 12,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            }),
            &mut state,
        );
        assert_eq!(state.last_usage.unwrap().input, 12);
    }
}
