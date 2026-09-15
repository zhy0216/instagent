//! CLI adaptation of the library task runner, plus session/plugin management.

use super::{output, render, OutputFormat, PluginAction, RunArgs, SessionsAction};
use anyhow::Context;
use instagent::agent::task::{self, Capabilities, RunRequest, TaskInput};
use instagent::plugin::install::{self, InstallSource};
use instagent::session::Session;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

pub async fn run(args: RunArgs) -> instagent::Result<ExitCode> {
    let input = match (args.task, args.task_file, args.command) {
        (Some(text), None, None) => TaskInput::Text(text),
        (None, Some(path), None) => TaskInput::File(path),
        (None, None, Some(name)) => TaskInput::Command {
            name,
            args: args.args.unwrap_or_default(),
        },
        _ => anyhow::bail!("exactly one task input is required"),
    };
    let request = RunRequest {
        input,
        resume: args.resume,
        cwd: args.cwd,
        provider: None,
        model: args.model,
        plugin_paths: args.plugin,
        timeout_secs: args.timeout,
        capabilities: Capabilities {
            plugins: args.only_plugin,
            tools: if args.no_tools {
                Some(Vec::new())
            } else {
                args.tool
            },
            required_tools: args.require_tool,
        },
    };
    let cancel = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let execution = task::run(request, cancel.clone(), tx);
    tokio::pin!(execution);
    let runner = async {
        tokio::select! {
            biased;
            signal = wait_for_signal() => {
                // Signal registration is polled before any initialization.
                cancel.cancel();
                let report = (&mut execution).await;
                signal.context("install signal handler")?;
                Ok::<_, anyhow::Error>(report)
            }
            report = &mut execution => Ok(report),
        }
    };
    let (report, ()) = tokio::join!(runner, render::print_events(rx, args.output));
    let report: task::RunReport = report?;
    for note in &report.diagnostics {
        let _ = writeln!(output::stderr(), "note: {}", note.message);
    }
    if let Some(error) = &report.error {
        let _ = writeln!(output::stderr(), "error: {error}");
    }
    let code = ExitCode::from(report.status.exit_code());
    if args.output == OutputFormat::Json {
        output::stdout()
            .report(report)
            .await
            .context("write JSON result")?;
    } else {
        let _ = output::stdout().finish(output::DELIVERY_TIMEOUT).await;
    }
    Ok(code)
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
        PluginAction::Install { source } => {
            let src = if Path::new(&source).is_dir() {
                InstallSource::Path(PathBuf::from(&source))
            } else {
                InstallSource::GitUrl(source.clone())
            };
            let plugin = install::install(&src).with_context(|| format!("install {source}"))?;
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
}
