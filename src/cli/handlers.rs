//! Noninteractive task execution, session management and plugin management.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use instagent::agent::TurnResult;
use instagent::hooks::{HookDecision, HookEvent};
use instagent::message::{Content, Role, Usage};
use instagent::plugin::install;
use instagent::plugin::install::{InstallOptions, InstallSource};
use instagent::session::Session;

use super::assembly::{self, AssemblyOpts};
use super::{render, OutputFormat, PluginAction, RunArgs, SessionsAction};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RunStatus {
    Completed,
    Failed,
    MaxTurns,
    TimedOut,
    Cancelled,
}

impl RunStatus {
    fn exit_code(self) -> ExitCode {
        ExitCode::from(match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::MaxTurns => 3,
            Self::TimedOut => 124,
            Self::Cancelled => 130,
        })
    }
}

/// One terminal result. Only a completed invocation has an answer; partial work
/// stays in the session, so failed resumes can never return an older answer.
#[derive(Serialize)]
struct RunReport {
    schema_version: u32,
    status: RunStatus,
    session_id: Option<String>,
    output: String,
    usage: Option<Usage>,
    error: Option<String>,
}

pub async fn run(args: RunArgs) -> instagent::Result<ExitCode> {
    let mut report = RunReport {
        schema_version: 1,
        status: RunStatus::Failed,
        session_id: None,
        output: String::new(),
        usage: None,
        error: None,
    };
    let cancel = CancellationToken::new();
    let mut interruption = None;
    let result = {
        let execution = execute(&args, &mut report, &cancel);
        tokio::pin!(execution);
        tokio::select! {
            biased;
            signal = wait_for_signal() => {
                interruption = Some(match signal {
                    Ok(()) => (RunStatus::Cancelled, "run cancelled".to_string()),
                    Err(err) => (RunStatus::Failed, format!("install signal handler: {err:#}")),
                });
                cancel.cancel();
                tokio::time::timeout(Duration::from_secs(5), &mut execution)
                    .await.unwrap_or_else(|_| Err(anyhow::anyhow!("cleanup timed out")))
            }
            _ = tokio::time::sleep(Duration::from_secs(args.timeout)) => {
                interruption = Some((RunStatus::TimedOut, format!("run timed out after {} seconds", args.timeout)));
                cancel.cancel();
                tokio::time::timeout(Duration::from_secs(5), &mut execution)
                    .await.unwrap_or_else(|_| Err(anyhow::anyhow!("cleanup timed out")))
            }
            result = &mut execution => result,
        }
    };
    let (status, error) = match interruption {
        Some((status, error)) => (status, Some(error)),
        None => match result {
            Ok(TurnResult::Done) => (RunStatus::Completed, None),
            Ok(TurnResult::Interrupted) => (RunStatus::Cancelled, Some("run cancelled".into())),
            Ok(TurnResult::MaxTurns) => (RunStatus::MaxTurns, Some("max turns reached".into())),
            Err(err) => (RunStatus::Failed, Some(format!("{err:#}"))),
        },
    };
    report.status = status;
    report.error = error;
    if !matches!(status, RunStatus::Completed) {
        report.output.clear();
        report.usage = None;
    }
    if let Some(error) = &report.error {
        eprintln!("error: {error}");
    }
    if args.output == OutputFormat::Json {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &report).context("write JSON result")?;
        writeln!(stdout).context("finish JSON result")?;
        stdout.flush().context("flush JSON result")?;
    }
    Ok(status.exit_code())
}

/// Register before execution is first polled, including during plugin startup.
async fn wait_for_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

async fn execute(
    args: &RunArgs,
    report: &mut RunReport,
    cancel: &CancellationToken,
) -> instagent::Result<TurnResult> {
    // Resolve input before any provider or plugin process is started.
    let input = read_task(args)?;
    let mut resumed = match args.resume.as_deref() {
        None => None,
        Some(id) => {
            if id == "last" && Session::list()?.is_empty() {
                anyhow::bail!("no session to resume");
            }
            Some(Session::open_or_resume(
                Some(id),
                &std::env::current_dir()?,
                "",
                "",
            )?)
        }
    };
    let cwd = if let Some(session) = &resumed {
        report.session_id = Some(session.header.id.clone());
        let original = session
            .header
            .cwd
            .canonicalize()
            .context("resolve saved session cwd")?;
        if let Some(cwd) = &args.cwd {
            if cwd.canonicalize().context("resolve --cwd")? != original {
                anyhow::bail!("--cwd differs from the resumed session working directory");
            }
        }
        original
    } else {
        resolve_cwd(args.cwd.clone())?
    };
    let opts = AssemblyOpts {
        cwd: cwd.clone(),
        model: args
            .model
            .clone()
            .or_else(|| resumed.as_ref().map(|s| s.header.model.clone())),
        provider: resumed.as_ref().map(|s| s.header.provider.clone()),
        cli_plugins: args.plugin.clone(),
    };
    let rt = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(TurnResult::Interrupted),
        result = assembly::build(&opts) => result?,
    };
    print_notes(&rt.notes, &mut std::io::stderr());

    let result = async {
        let task = match input {
            Some(task) => task,
            None => {
                let name = args.command.as_deref().context("task input is required")?;
                let template = rt.task_templates.iter().find(|template| template.name == name)
                    .with_context(|| format!("unknown task template `{name}`; use plugin:name from an enabled plugin"))?;
                instagent::commands::expand(template, args.args.as_deref().unwrap_or(""))
            }
        };
        if task.trim().is_empty() {
            anyhow::bail!("task must not be empty or whitespace");
        }
        let mut session = match resumed.take() {
            Some(session) => session,
            None => Session::create(&cwd, &rt.provider_name, &rt.model)?,
        };
        report.session_id = Some(session.header.id.clone());
        eprintln!("session {}", session.header.id);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {},
            result = rt.agent.run_session_event(HookEvent::SessionStart, &session) => {
                report_session_hook(HookEvent::SessionStart, result, &mut std::io::stderr());
            }
        }
        let result = run_one_turn(&rt.agent, &mut session, task, cancel.clone(), args.output).await;
        if matches!(&result, Ok(TurnResult::Done)) {
            // The loop guarantees Done follows a new, nonempty terminal answer.
            if let Some(message) = session.messages.last().filter(|m| m.role == Role::Assistant) {
                report.output = message.content.iter().filter_map(|content| match content {
                    Content::Text(text) => Some(text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join("");
                report.usage = message.usage;
            }
        }
        // Normal hooks remain deadline-bound by the outer runner. After a
        // cancellation give lifecycle hooks a small, explicit cleanup budget.
        let end = rt.agent.run_session_event(HookEvent::SessionEnd, &session);
        tokio::pin!(end);
        let end_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => tokio::time::timeout(Duration::from_secs(2), &mut end)
                .await.map_err(anyhow::Error::from).and_then(|result| result),
            result = &mut end => result,
        };
        report_session_hook(HookEvent::SessionEnd, end_result, &mut std::io::stderr());
        result
    }.await;
    if tokio::time::timeout(Duration::from_secs(3), rt.agent.tools.shutdown())
        .await
        .is_err()
    {
        eprintln!("warning: tool shutdown timed out");
    }
    result
}

fn read_task(args: &RunArgs) -> instagent::Result<Option<String>> {
    const MAX_TASK_BYTES: u64 = 1024 * 1024;
    let count = usize::from(args.task.is_some())
        + usize::from(args.task_file.is_some())
        + usize::from(args.command.is_some());
    if count != 1 {
        anyhow::bail!("exactly one task input is required");
    }
    let task = if let Some(path) = &args.task_file {
        if !std::fs::metadata(path)
            .with_context(|| format!("read task file {}", path.display()))?
            .is_file()
        {
            anyhow::bail!("task file must be a regular UTF-8 file");
        }
        let file = std::fs::File::open(path).context("open task file")?;
        let mut text = String::new();
        file.take(MAX_TASK_BYTES + 1)
            .read_to_string(&mut text)
            .context("read UTF-8 task file")?;
        Some(text)
    } else {
        args.task.clone()
    };
    if let Some(task) = &task {
        if task.trim().is_empty() {
            anyhow::bail!("task must not be empty or whitespace");
        }
        if task.len() as u64 > MAX_TASK_BYTES {
            anyhow::bail!("task exceeds the 1 MiB input limit");
        }
    }
    Ok(task)
}

async fn run_one_turn(
    agent: &instagent::agent::Agent,
    session: &mut Session,
    task: String,
    cancel: CancellationToken,
    output: OutputFormat,
) -> instagent::Result<TurnResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    // Both futures are owned by this invocation; cancellation cannot detach a
    // printer that might write after the terminal JSON result.
    let (result, ()) = tokio::join!(
        agent.run_turn(session, task, cancel, tx),
        render::print_events(rx, output),
    );
    result
}

/// SessionStart / SessionEnd hook 结果处理（todo 11 / A4，ADR 0003 D3）：
/// 失败只输出一行含事件（阶段）与来源（错误链上下文）的 stderr warning，
/// 不改退出码、不打断会话——保持默认兼容。hook 执行内部失败（spawn /
/// 超时 / 输出超限 / 无决策）已由 `hooks.rs` 按 D3 逐条产出带插件与命令的
/// warning；不可阻止的会话事件若仍带回 Block/None 决策，同样按 fail-open
/// 放行并 warning。纯函数，便于稳定断言。
fn report_session_hook(
    event: HookEvent,
    result: instagent::Result<HookDecision>,
    out: &mut dyn Write,
) {
    let warning = match result {
        Ok(HookDecision::Allow) => return,
        Ok(decision) => format!(
            "warning: {event} hook returned {decision:?} on a non-blockable event; \
             ignored (fail-open)"
        ),
        Err(err) => format!("warning: {event} hook failed: {err:#}"),
    };
    let _ = writeln!(out, "{warning}");
}

fn resolve_cwd(cwd: Option<PathBuf>) -> instagent::Result<PathBuf> {
    match cwd {
        Some(dir) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create cwd {}", dir.display()))?;
            Ok(dir.canonicalize()?)
        }
        None => Ok(std::env::current_dir()?),
    }
}

fn print_notes(notes: &[String], out: &mut dyn Write) {
    for note in notes {
        let _ = writeln!(out, "note: {note}");
    }
}

/// `instagent sessions list | rm <id>`。
pub fn sessions(action: SessionsAction) -> instagent::Result<()> {
    let mut out = std::io::stdout();
    match action {
        SessionsAction::List => {
            for line in sessions_list_rows()? {
                writeln!(out, "{line}")?;
            }
        }
        SessionsAction::Rm { id } => {
            Session::remove(&id).with_context(|| format!("remove session {id}"))?;
            writeln!(out, "removed session {id}")?;
        }
    }
    Ok(())
}

/// list 的行渲染（纯数据 → 文本，便于测试）。
pub fn sessions_list_rows() -> instagent::Result<Vec<String>> {
    let headers = Session::list()?;
    let mut rows = Vec::with_capacity(headers.len());
    for (index, header) in headers.iter().enumerate() {
        let created = chrono::DateTime::from_timestamp(header.created, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| header.created.to_string());
        rows.push(format!(
            "{:>3}. {}  {created}  {}/{}  cwd={}",
            index + 1,
            header.id,
            header.provider,
            header.model,
            header.cwd.display()
        ));
    }
    if rows.is_empty() {
        rows.push("(no sessions)".to_string());
    }
    Ok(rows)
}

/// `instagent plugin ...` 子命令（接 `07` 数据层）。
pub fn plugin(action: PluginAction) -> instagent::Result<()> {
    let cwd = std::env::current_dir()?;
    let mut out = std::io::stdout();
    match action {
        PluginAction::Install {
            source,
            auto_update,
        } => {
            let src = if Path::new(&source).is_dir() {
                InstallSource::Path(PathBuf::from(&source))
            } else {
                InstallSource::GitUrl(source.clone())
            };
            let plugin = install::install(&src, &InstallOptions { auto_update })
                .with_context(|| format!("install {source}"))?;
            writeln!(
                out,
                "installed `{}` v{} at {}",
                plugin.manifest.name,
                plugin.manifest.version,
                plugin.root.display()
            )?;
        }
        PluginAction::List => {
            let installed = install::list(&cwd)?;
            if installed.is_empty() {
                writeln!(out, "(no plugins installed)")?;
            }
            for item in installed {
                let source = item
                    .install_info
                    .as_ref()
                    .map(|info| info.source.clone())
                    .unwrap_or_else(|| "manual".to_string());
                writeln!(
                    out,
                    "{}  v{}  {}  {source}",
                    item.plugin.manifest.name,
                    item.plugin.manifest.version,
                    if item.enabled { "enabled" } else { "disabled" }
                )?;
            }
        }
        PluginAction::Update { name } => {
            let targets: Vec<String> = match name {
                Some(name) => vec![name],
                None => install::list(&cwd)?
                    .into_iter()
                    .filter(|item| {
                        item.install_info
                            .as_ref()
                            .is_some_and(|info| info.commit.is_some())
                    })
                    .map(|item| item.plugin.manifest.name)
                    .collect(),
            };
            if targets.is_empty() {
                writeln!(out, "(no git-sourced plugins to update)")?;
            }
            let mut failures = Vec::new();
            let mut diag = std::io::stderr();
            for target in targets {
                match install::update(&target) {
                    Ok(()) => writeln!(out, "updated {target}")?,
                    Err(err) => {
                        let _ = writeln!(diag, "error: update {target} failed: {err:#}");
                        failures.push(target);
                    }
                }
            }
            if !failures.is_empty() {
                anyhow::bail!("plugin update failed for: {}", failures.join(", "));
            }
        }
        PluginAction::Enable { name } => {
            install::enable(&name).with_context(|| format!("enable {name}"))?;
            writeln!(out, "enabled {name}")?;
        }
        PluginAction::Disable { name } => {
            install::disable(&name).with_context(|| format!("disable {name}"))?;
            writeln!(out, "disabled {name}")?;
        }
        PluginAction::Show { name } => {
            let item = install::show(&cwd, &name)?;
            let manifest = &item.plugin.manifest;
            writeln!(out, "name: {}", manifest.name)?;
            writeln!(out, "version: {}", manifest.version)?;
            if let Some(description) = &manifest.description {
                writeln!(out, "description: {description}")?;
            }
            if let Some(author) = &manifest.author {
                let name = match author {
                    instagent::plugin::manifest::Author::Name(n) => n.clone(),
                    instagent::plugin::manifest::Author::Detailed { name, .. } => name.clone(),
                };
                writeln!(out, "author: {name}")?;
            }
            writeln!(out, "root: {}", item.plugin.root.display())?;
            writeln!(out, "enabled: {}", item.enabled)?;
            match &item.install_info {
                Some(info) => {
                    writeln!(out, "source: {}", info.source)?;
                    writeln!(
                        out,
                        "commit: {}",
                        info.commit
                            .as_deref()
                            .map(|c| c.chars().take(8).collect::<String>())
                            .unwrap_or_else(|| "-".into())
                    )?;
                    writeln!(out, "auto-update: {}", info.auto_update)?;
                }
                None => writeln!(out, "source: (manual copy)")?,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::fixtures::Env;

    #[test]
    fn sessions_list_rows_and_rm() {
        let env = Env::new();
        assert_eq!(sessions_list_rows().unwrap(), vec!["(no sessions)"]);
        let a = Session::create(env.cwd.path(), "fake", "m1").unwrap();
        let b = Session::create(env.cwd.path(), "fake", "m2").unwrap();
        let rows = sessions_list_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.contains(&a.header.id) && r.contains("fake/m1")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains(&b.header.id)), "{rows:?}");

        sessions(SessionsAction::Rm {
            id: b.header.id.clone(),
        })
        .unwrap();
        assert!(!b.path.exists());
        assert!(sessions_list_rows()
            .unwrap()
            .iter()
            .all(|r| !r.contains(&b.header.id)));
        assert!(sessions(SessionsAction::Rm { id: "nope".into() }).is_err());
    }

    // ---- session hook 失败可见性（todo 11 / A4，ADR 0003 D3） ----

    #[test]
    fn session_hook_allow_is_silent() {
        let mut out = Vec::new();
        report_session_hook(HookEvent::SessionStart, Ok(HookDecision::Allow), &mut out);
        assert!(out.is_empty(), "正常 session 默认零输出（兼容性不变）");
    }

    #[test]
    fn session_hook_error_warns_with_phase_and_source() {
        let err = anyhow::anyhow!("failed to spawn: no such file")
            .context("SessionStart hook of plugin brokenplug");
        let mut out = Vec::new();
        report_session_hook(HookEvent::SessionStart, Err(err), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "warning: SessionStart hook failed: SessionStart hook of plugin brokenplug: \
             failed to spawn: no such file\n"
        );
    }

    #[test]
    fn session_hook_unexpected_decision_warns_but_passes() {
        // 会话事件不可阻止：Block/None 决策按 fail-open 放行，但必须可见。
        let mut out = Vec::new();
        report_session_hook(
            HookEvent::SessionEnd,
            Ok(HookDecision::Block("policy".into())),
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "warning: SessionEnd hook returned Block(\"policy\") on a non-blockable event; \
             ignored (fail-open)\n"
        );
    }
}
