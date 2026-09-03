//! REPL（第二版 §2.11）：rustyline 循环 + 斜杠命令 + Ctrl-C。
//!
//! Ctrl-C：第一次取消当前轮（cancel token），第二次退出；空闲时第一次
//! 提示、第二次退出。斜杠命令：`/exit` `/clear` `/compact` `/mode <m>`
//! `/tools` `/help` + `17` 的插件斜杠命令（动态展开进 prompt）。

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
use instagent::config::Mode;
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
    Mode(Mode),
    Exit,
    /// 未知斜杠命令（携带命令名）。
    Unknown(String),
    /// `mode` 参数非法。
    BadMode(String),
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
        "mode" => match args.parse() {
            Ok(mode) => Input::Mode(mode),
            Err(_) => Input::BadMode(args.to_string()),
        },
        _ => {
            if let Some(cmd) = plugin_commands.iter().find(|c| c.name == name) {
                Input::Prompt(expand(cmd, args))
            } else {
                Input::Unknown(name.to_string())
            }
        }
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => "auto",
        Mode::Approve => "approve",
        Mode::Chat => "chat",
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

    println!(
        "instagent · provider {} · model {} · mode {} · session {}\n(type /help, Ctrl-D or /exit to quit)",
        rt.provider_name,
        rt.model,
        mode_name(rt.agent.cfg.mode),
        session.header.id
    );

    // 接 CLI 审批通道（approve 模式用；`16` 的 Confirm trait）。
    rt.agent.approval.confirm = Some(Arc::new(render::CliConfirm));

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
                    println!();
                    break;
                }
                println!("(Ctrl-C again or /exit to quit)");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!();
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
            Input::Help => print_help(&rt.slash_commands, rt.agent.cfg.mode),
            Input::Clear => {
                session.rewrite(Vec::new())?;
                println!("(context cleared)");
            }
            Input::Compact => {
                compact_now(rt, session).await?;
            }
            Input::Tools => {
                for spec in rt.agent.tools.list().await {
                    println!(
                        "{}{} — {}",
                        spec.name,
                        if spec.read_only { " (read-only)" } else { "" },
                        one_line(&spec.description)
                    );
                }
            }
            Input::Mode(mode) => {
                rt.agent.cfg.mode = mode;
                rt.agent.approval.mode = mode;
                println!("(mode = {})", mode_name(mode));
            }
            Input::BadMode(arg) => {
                println!("usage: /mode auto|approve|chat (got {arg:?})");
            }
            Input::Unknown(name) => {
                println!("unknown command /{name}; /help lists commands");
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
        TurnResult::Interrupted => println!("(turn cancelled; Ctrl-C again to quit)"),
        TurnResult::MaxTurns => println!("(max turns reached)"),
    }
    Ok(quit.load(Ordering::SeqCst))
}

pub(crate) async fn print_events(mut rx: mpsc::Receiver<instagent::agent::Event>) {
    let mut state = render::RenderState::default();
    while let Some(event) = rx.recv().await {
        render::render_event(&event, &mut state);
    }
    render::finish_turn(&mut state);
}

/// 轮内 Ctrl-C：第一次取消当前轮，第二次请求退出。
async fn watch_ctrl_c(cancel: CancellationToken, quit: Arc<AtomicBool>) {
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        if cancel.is_cancelled() {
            quit.store(true, Ordering::SeqCst);
            println!("\n^C quitting");
            return;
        }
        cancel.cancel();
        println!("\n^C cancelling current turn (press Ctrl-C again to quit)");
    }
}

/// `/compact`：立即强制压缩（`16` 的 compact::force）。
async fn compact_now(rt: &mut Runtime, session: &mut Session) -> instagent::Result<()> {
    let (tx, rx) = mpsc::channel::<instagent::agent::Event>(64);
    let printer = tokio::spawn(print_events(rx));
    compact::force(&rt.agent, session, &tx).await?;
    drop(tx);
    let _ = printer.await;
    println!("(compacted)");
    Ok(())
}

fn print_help(plugin_commands: &[SlashCommand], mode: Mode) {
    println!(
        "/exit /quit  quit\n\
         /clear       drop conversation context\n\
         /compact     force compaction now\n\
         /mode <m>    auto | approve | chat (current: {})\n\
         /tools       list visible tools\n\
         /help        this message",
        mode_name(mode)
    );
    if plugin_commands.is_empty() {
        return;
    }
    println!("plugin commands:");
    for cmd in plugin_commands {
        let hint = cmd.argument_hint.as_deref().unwrap_or("");
        let desc = cmd.description.as_deref().map(one_line).unwrap_or_default();
        println!("  /{} {hint}— {desc}", cmd.name);
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
    fn mode_parsing_is_case_insensitive_and_strict() {
        let cmds = [];
        assert_eq!(classify("/mode AUTO", &cmds), Input::Mode(Mode::Auto));
        assert_eq!(classify("/mode chat", &cmds), Input::Mode(Mode::Chat));
        assert_eq!(classify("/mode", &cmds), Input::BadMode(String::new()));
        assert_eq!(classify("/mode yolo", &cmds), Input::BadMode("yolo".into()));
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
