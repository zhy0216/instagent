//! REPL（第二版 §2.11）：rustyline 循环 + 斜杠命令 + Ctrl-C。
//!
//! Ctrl-C：第一次取消当前轮（cancel token），第二次退出；空闲时第一次
//! 提示、第二次退出。斜杠命令：`/exit` `/clear` `/compact` `/tools`
//! `/help` + `17` 的插件斜杠命令（动态展开进 prompt）。
//!
//! 输出契约（ADR 0003 D4）：stdout 只有模型回答文本流；横幅、斜杠命令
//! 反馈、Ctrl-C 提示等一切诊断走 stderr。

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Context;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use instagent::agent::compact;
use instagent::agent::TurnResult;
use instagent::commands::expand;
use instagent::commands::SlashCommand;
use instagent::session::Session;

use super::assembly::Runtime;
use super::render;

/// 一行输入的归类（纯函数 [`classify`] 的产物，便于测试）。
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// 普通文本或插件斜杠命令展开后的 prompt。
    Prompt(String),
    Clear,
    Compact,
    Help,
    Tools,
    Exit,
    /// 未知斜杠命令（携带命令名）。
    Unknown(String),
}

/// 斜杠命令分发（纯函数：不依赖 editor / agent）。
pub fn classify(line: &str, plugin_commands: &[SlashCommand]) -> Input {
    let line = line.trim();
    if !line.starts_with('/') {
        return Input::Prompt(line.to_string());
    }
    let body = &line[1..];
    let (name, args) = match body.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (body, ""),
    };
    match name {
        "exit" | "quit" => Input::Exit,
        "help" => Input::Help,
        "clear" => Input::Clear,
        "compact" => Input::Compact,
        "tools" => Input::Tools,
        _ => {
            if let Some(cmd) = plugin_commands.iter().find(|c| c.name == name) {
                Input::Prompt(expand(cmd, args))
            } else {
                Input::Unknown(name.to_string())
            }
        }
    }
}

/// REPL 主循环；返回时调用方负责 SessionEnd hook 与工具源 shutdown。
pub async fn chat_loop(rt: &mut Runtime, session: &mut Session) -> instagent::Result<()> {
    let mut editor = DefaultEditor::new().context("init rustyline")?;
    let history = history_path()?;
    if let Some(parent) = history.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = editor.load_history(&history);

    eprintln!(
        "instagent · provider {} · model {} · session {}\n(type /help, Ctrl-D or /exit to quit)",
        rt.provider_name, rt.model, session.header.id
    );

    let mut quit = false;
    let mut idle_ctrl_c = 0u32;
    while !quit {
        let line = match editor.readline("instagent> ") {
            Ok(line) => {
                idle_ctrl_c = 0;
                line
            }
            Err(ReadlineError::Interrupted) => {
                idle_ctrl_c += 1;
                if idle_ctrl_c >= 2 {
                    eprintln!();
                    break;
                }
                eprintln!("(Ctrl-C again or /exit to quit)");
                continue;
            }
            Err(ReadlineError::Eof) => {
                eprintln!();
                break;
            }
            Err(err) => return Err(err).context("readline"),
        };
        let raw = line.trim().to_string();
        if raw.is_empty() {
            continue;
        }
        if !raw.starts_with('/') {
            let _ = editor.add_history_entry(&raw);
        }
        match classify(&raw, &rt.slash_commands) {
            Input::Prompt(text) => {
                quit = run_turn(rt, session, text).await?;
            }
            Input::Exit => break,
            Input::Help => print_help(&rt.slash_commands),
            Input::Clear => {
                session.rewrite(Vec::new())?;
                eprintln!("(context cleared)");
            }
            Input::Compact => {
                compact_now(rt, session).await?;
            }
            Input::Tools => {
                for spec in rt.agent.tools.list().await {
                    eprintln!(
                        "{}{} — {}",
                        spec.name,
                        if spec.read_only { " (read-only)" } else { "" },
                        one_line(&spec.description)
                    );
                }
            }
            Input::Unknown(name) => {
                eprintln!("unknown command /{name}; /help lists commands");
            }
        }
    }
    let _ = editor.save_history(&history);
    Ok(())
}

/// 跑一轮：事件打印机任务 + Ctrl-C 观察任务。返回是否应退出 REPL。
async fn run_turn(
    rt: &mut Runtime,
    session: &mut Session,
    text: String,
) -> instagent::Result<bool> {
    let cancel = CancellationToken::new();
    let quit = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<instagent::agent::Event>(256);
    let printer = tokio::spawn(print_events(rx));
    let watcher = tokio::spawn(watch_ctrl_c(cancel.clone(), quit.clone()));

    let result = rt.agent.run_turn(session, text, cancel.clone(), tx).await;

    drop(cancel);
    watcher.abort();
    let _ = printer.await;

    match result? {
        TurnResult::Done => {}
        TurnResult::Interrupted => eprintln!("(turn cancelled; Ctrl-C again to quit)"),
        TurnResult::MaxTurns => eprintln!("(max turns reached)"),
    }
    Ok(quit.load(Ordering::SeqCst))
}

pub(crate) async fn print_events(mut rx: mpsc::Receiver<instagent::agent::Event>) {
    let mut state = render::RenderState::default();
    let mut out = std::io::stdout();
    let mut diag = std::io::stderr();
    while let Some(event) = rx.recv().await {
        render::render_event(&event, &mut state, &mut out, &mut diag);
    }
    render::finish_turn(&mut state, &mut out, &mut diag);
}

/// 轮内 Ctrl-C：第一次取消当前轮，第二次请求退出。
async fn watch_ctrl_c(cancel: CancellationToken, quit: Arc<AtomicBool>) {
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        if cancel.is_cancelled() {
            quit.store(true, Ordering::SeqCst);
            eprintln!("\n^C quitting");
            return;
        }
        cancel.cancel();
        eprintln!("\n^C cancelling current turn (press Ctrl-C again to quit)");
    }
}

/// `/compact`：立即强制压缩（`16` 的 [`compact::force_cancelable`]，可取消）。
/// Ctrl-C 取消当前压缩：printer/watcher 正确收尾，REPL 可继续，会话不动；
/// 无可压缩历史时同样 no-op；错误返回也不遗留打印任务（todo 08 / A04）。
async fn compact_now(rt: &mut Runtime, session: &mut Session) -> instagent::Result<()> {
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel::<instagent::agent::Event>(64);
    let printer = tokio::spawn(print_events(rx));
    let watcher = tokio::spawn(watch_compact_ctrl_c(cancel.clone()));
    let result = compact::force_cancelable(&rt.agent, session, &tx, &cancel).await;
    let cancelled = cancel.is_cancelled();
    drop(tx);
    drop(cancel);
    watcher.abort();
    let _ = printer.await;
    match result? {
        true => eprintln!("(compacted)"),
        false if cancelled => eprintln!("(compaction cancelled)"),
        false => eprintln!("(nothing to compact)"),
    }
    Ok(())
}

/// 压缩内 Ctrl-C：只取消当前压缩，REPL 继续（与轮内两段式不同，压缩取消
/// 后不直接退出；第二次 Ctrl-C 由下一轮的 `watch_ctrl_c` 处理）。
async fn watch_compact_ctrl_c(cancel: CancellationToken) {
    if tokio::signal::ctrl_c().await.is_ok() {
        cancel.cancel();
        eprintln!("\n^C cancelling compaction");
    }
}

fn print_help(plugin_commands: &[SlashCommand]) {
    eprintln!(
        "/exit /quit  quit\n\
         /clear       drop conversation context\n\
         /compact     force compaction now\n\
         /tools       list visible tools\n\
         /help        this message"
    );
    if plugin_commands.is_empty() {
        return;
    }
    eprintln!("plugin commands:");
    for cmd in plugin_commands {
        let hint = cmd.argument_hint.as_deref().unwrap_or("");
        let desc = cmd.description.as_deref().map(one_line).unwrap_or_default();
        eprintln!("  /{} {hint}— {desc}", cmd.name);
    }
}

fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

fn history_path() -> instagent::Result<std::path::PathBuf> {
    Ok(instagent::config::config_dir()?.join("history.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, template: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: Some(format!("fake {name}")),
            argument_hint: None,
            template: template.into(),
        }
    }

    #[test]
    fn plain_text_and_builtins_dispatch() {
        let cmds = [];
        assert_eq!(classify("hello", &cmds), Input::Prompt("hello".into()));
        assert_eq!(
            classify("  spaced  ", &cmds),
            Input::Prompt("spaced".into())
        );
        assert_eq!(classify("/exit", &cmds), Input::Exit);
        assert_eq!(classify("/quit now", &cmds), Input::Exit);
        assert_eq!(classify("/help", &cmds), Input::Help);
        assert_eq!(classify("/clear", &cmds), Input::Clear);
        assert_eq!(classify("/compact", &cmds), Input::Compact);
        assert_eq!(classify("/tools", &cmds), Input::Tools);
    }

    #[test]
    fn plugin_commands_expand_like_prompts() {
        let cmds = [cmd("review", "Review this: $ARGUMENTS")];
        assert_eq!(
            classify("/review security", &cmds),
            Input::Prompt("Review this: security".into())
        );
        // 未知命令给可读名字。
        assert_eq!(classify("/nope x", &cmds), Input::Unknown("nope".into()));
    }
}
