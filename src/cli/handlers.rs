//! 子命令处理：chat / run / sessions / plugin（第二版 §2.11，plugin 见第三版 §2.10）。

use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

use instagent::agent::TurnResult;
use instagent::config::Mode;
use instagent::hooks::HookEvent;
use instagent::plugin::install;
use instagent::plugin::install::InstallOptions;
use instagent::plugin::install::InstallSource;
use instagent::plugin::Plugin;
use instagent::session::Session;

use super::assembly;
use super::assembly::AssemblyOpts;
use super::assembly::Prompter;
use super::repl;
use super::trust;
use super::PluginAction;
use super::SessionsAction;

/// `instagent chat`：交互式 REPL（会话生命周期触发 SessionStart/End hooks）。
pub async fn chat(
    resume: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
    mode: Option<Mode>,
    plugin: Vec<PathBuf>,
) -> instagent::Result<()> {
    let cwd = resolve_cwd(cwd)?;
    let opts = AssemblyOpts {
        cwd: cwd.clone(),
        model,
        mode,
        cli_plugins: plugin,
        assume_yes: false,
        interactive: true,
    };
    // 信任确认在 REPL 启动前完成；提前释放 stdin 锁，避免与 rustyline 抢缓冲。
    let mut rt = {
        let stdin = std::io::stdin();
        let mut reader: Box<dyn BufRead> = Box::new(stdin.lock());
        let mut output = std::io::stdout();
        let mut prompter = Prompter {
            input: &mut reader,
            output: &mut output,
        };
        assembly::build(&opts, &mut prompter).await?
    };
    print_notes(&rt.notes, &mut std::io::stdout());

    let mut session =
        Session::open_or_resume(resume.as_deref(), &cwd, &rt.provider_name, &rt.model)
            .with_context(|| format!("open session {resume:?}"))?;

    let _ = rt
        .agent
        .run_session_event(HookEvent::SessionStart, &session)
        .await;
    let result = repl::chat_loop(&mut rt, &mut session).await;
    let _ = rt
        .agent
        .run_session_event(HookEvent::SessionEnd, &session)
        .await;
    rt.agent.tools.shutdown().await;
    result
}

/// `instagent run -t "..."`：无交互跑一条任务；审批按 auto，打印最终回复和 usage。
pub async fn run(
    task: String,
    cwd: Option<PathBuf>,
    model: Option<String>,
    mode: Option<Mode>,
    plugin: Vec<PathBuf>,
) -> instagent::Result<()> {
    let cwd = resolve_cwd(cwd)?;
    let opts = AssemblyOpts {
        cwd: cwd.clone(),
        model,
        mode: Some(mode.unwrap_or(Mode::Auto)),
        cli_plugins: plugin,
        assume_yes: false,
        interactive: false,
    };
    let mut stderr = std::io::stderr();
    let rt = {
        let stdin = std::io::stdin();
        let mut reader: Box<dyn BufRead> = Box::new(stdin.lock());
        let mut prompter = Prompter {
            input: &mut reader,
            output: &mut stderr,
        };
        assembly::build(&opts, &mut prompter).await?
    };
    print_notes(&rt.notes, &mut stderr);

    let mut session = Session::create(&cwd, &rt.provider_name, &rt.model)?;
    eprintln!("session {}", session.header.id);

    let _ = rt
        .agent
        .run_session_event(HookEvent::SessionStart, &session)
        .await;
    let result = run_one_turn(&rt.agent, &mut session, task).await;
    let _ = rt
        .agent
        .run_session_event(HookEvent::SessionEnd, &session)
        .await;
    rt.agent.tools.shutdown().await;

    match result? {
        TurnResult::Done => {}
        TurnResult::Interrupted => eprintln!("(interrupted)"),
        TurnResult::MaxTurns => eprintln!("(max turns reached)"),
    }
    Ok(())
}

async fn run_one_turn(
    agent: &instagent::agent::Agent,
    session: &mut Session,
    task: String,
) -> instagent::Result<TurnResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let printer = tokio::spawn(repl::print_events(rx));
    let result = agent
        .run_turn(
            session,
            task,
            tokio_util::sync::CancellationToken::new(),
            tx,
        )
        .await;
    let _ = printer.await;
    result
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

/// `instagent plugin ...` 子命令（接 `07` 数据层 + 信任确认）。
pub fn plugin(action: PluginAction) -> instagent::Result<()> {
    let cwd = std::env::current_dir()?;
    let stdin = std::io::stdin();
    let mut reader: Box<dyn BufRead> = Box::new(stdin.lock());
    let mut out = std::io::stdout();
    match action {
        PluginAction::Install {
            source,
            auto_update,
            yes,
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
            confirm_trust(&plugin, yes, &mut reader, &mut out)?;
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
            for target in targets {
                match install::update(&target) {
                    Ok(()) => writeln!(out, "updated {target}")?,
                    Err(err) => writeln!(out, "update {target} failed: {err:#}")?,
                }
            }
        }
        PluginAction::Enable { name, yes } => {
            install::enable(&name).with_context(|| format!("enable {name}"))?;
            writeln!(out, "enabled {name}")?;
            let item = install::show(&cwd, &name)?;
            confirm_trust(&item.plugin, yes, &mut reader, &mut out)?;
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
            let trusted = trust::user_trusted()?.iter().any(|t| t == &manifest.name);
            writeln!(out, "trusted: {trusted}")?;
            let surfaces = trust::plugin_surfaces(&item.plugin)?;
            if surfaces.is_empty() {
                writeln!(out, "commands: (none)")?;
            } else {
                writeln!(out, "commands:")?;
                for surface in surfaces {
                    writeln!(out, "  [{}] {}", surface.kind, surface.command)?;
                }
            }
        }
    }
    Ok(())
}

/// 安装 / 启用后的首次信任确认（`--yes` 跳过）。
fn confirm_trust(
    plugin: &Plugin,
    yes: bool,
    reader: &mut dyn BufRead,
    out: &mut dyn Write,
) -> instagent::Result<()> {
    let mut trusted = trust::user_trusted()?;
    let granted = trust::ensure_trusted(
        plugin,
        &trust::plugin_surfaces(plugin)?,
        &mut trusted,
        yes,
        reader,
        out,
    )?;
    if granted && !plugin_surfaces_empty(plugin)? {
        writeln!(out, "plugin `{}` trusted", plugin.manifest.name)?;
    }
    Ok(())
}

fn plugin_surfaces_empty(plugin: &Plugin) -> instagent::Result<bool> {
    Ok(trust::plugin_surfaces(plugin)?.is_empty())
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
}
