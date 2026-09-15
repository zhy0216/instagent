//! Complete unattended task lifecycle shared by CLI and library hosts. The host
//! supplies cancellation and consumes optional progress; this module installs
//! no signal handlers and writes no terminal output. Filesystem configuration
//! and session directories retain the existing INSTAGENT_* conventions.

mod assembly;

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{event, Event, TurnResult};
use crate::hooks::{HookDecision, HookEvent};
use crate::message::{Content, Role, Usage};
use crate::session::Session;

#[derive(Debug, Clone)]
pub enum TaskInput {
    Text(String),
    File(PathBuf),
    Command { name: String, args: String },
}

/// Optional allowlists narrow already-enabled plugins and model-visible tool
/// names. Empty lists select nothing. Required tools must also be allowed.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub plugins: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub required_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub input: TaskInput,
    pub resume: Option<String>,
    pub cwd: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_paths: Vec<PathBuf>,
    pub capabilities: Capabilities,
    /// 1–604800 seconds, including initialization; cleanup gets at most 5 more.
    pub timeout_secs: u64,
}

impl RunRequest {
    pub fn new(input: TaskInput) -> Self {
        Self {
            input,
            resume: None,
            cwd: None,
            provider: None,
            model: None,
            plugin_paths: Vec::new(),
            capabilities: Capabilities::default(),
            timeout_secs: 600,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Failed,
    MaxTurns,
    TimedOut,
    Cancelled,
}

impl RunStatus {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::MaxTurns => 3,
            Self::TimedOut => 124,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    PluginSkipped,
    PluginUnavailable,
    McpUnavailable,
    ToolInventory,
    RequiredToolMissing,
    Configuration,
    SessionHook,
    ToolShutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub status: RunStatus,
    pub session_id: Option<String>,
    pub output: String,
    pub usage: Option<Usage>,
    pub error: Option<String>,
    /// Additive schema-v1 field, omitted on healthy runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Execute once, preserving the same terminal states and session invariants for
/// every host. Dropping/ignoring the progress receiver does not lose the result.
/// Cancel the token and await the report to allow bounded lifecycle cleanup.
///
/// ```no_run
/// use instagent::agent::task::{self, RunRequest, TaskInput};
/// use tokio_util::sync::CancellationToken;
/// # async fn example() {
/// let mut request = RunRequest::new(TaskInput::Text("Inspect this project".into()));
/// request.capabilities.tools = Some(vec!["read".into(), "tree".into()]);
/// let (events, progress) = tokio::sync::mpsc::channel(128);
/// drop(progress);
/// let report = task::run(request, CancellationToken::new(), events).await;
/// let exit_code = report.status.exit_code();
/// # }
/// ```
pub async fn run(
    request: RunRequest,
    cancellation: CancellationToken,
    events: mpsc::Sender<Event>,
) -> RunReport {
    let mut report = RunReport {
        schema_version: 1,
        status: RunStatus::Failed,
        session_id: None,
        output: String::new(),
        usage: None,
        error: None,
        diagnostics: Vec::new(),
    };
    if !(1..=604800).contains(&request.timeout_secs) {
        report.error = Some("timeout must be between 1 and 604800 seconds".into());
        return report;
    }
    let cancel = cancellation.child_token();
    let mut interruption = None;
    let result = {
        let execution = execute(&request, &mut report, &cancel, events);
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                interruption = Some((RunStatus::Cancelled, "run cancelled".to_string()));
                cancel.cancel();
                tokio::time::timeout(Duration::from_secs(5), &mut execution).await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("cleanup timed out")))
            }
            _ = tokio::time::sleep(Duration::from_secs(request.timeout_secs)) => {
                interruption = Some((RunStatus::TimedOut, format!("run timed out after {} seconds", request.timeout_secs)));
                cancel.cancel();
                tokio::time::timeout(Duration::from_secs(5), &mut execution).await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("cleanup timed out")))
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
    if status != RunStatus::Completed {
        report.output.clear();
        report.usage = None;
    }
    report
}

async fn execute(
    request: &RunRequest,
    report: &mut RunReport,
    cancel: &CancellationToken,
    events: mpsc::Sender<Event>,
) -> crate::Result<TurnResult> {
    if cancel.is_cancelled() {
        return Ok(TurnResult::Interrupted);
    }
    let input = read_task(&request.input)?;
    let mut resumed = match request.resume.as_deref() {
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
        if let Some(cwd) = &request.cwd {
            if cwd.canonicalize().context("resolve --cwd")? != original {
                anyhow::bail!("--cwd differs from the resumed session working directory");
            }
        }
        original
    } else {
        match &request.cwd {
            Some(dir) => {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create cwd {}", dir.display()))?;
                dir.canonicalize()?
            }
            None => std::env::current_dir()?,
        }
    };
    let opts = assembly::AssemblyOpts {
        cwd: cwd.clone(),
        model: request
            .model
            .clone()
            .or_else(|| resumed.as_ref().map(|s| s.header.model.clone())),
        provider: resumed
            .as_ref()
            .map(|s| s.header.provider.clone())
            .or_else(|| request.provider.clone()),
        cli_plugins: request.plugin_paths.clone(),
        capabilities: request.capabilities.clone(),
    };
    let rt = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(TurnResult::Interrupted),
        result = assembly::build(&opts, &mut report.diagnostics) => result?,
    };
    let result = async {
        let specs = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(TurnResult::Interrupted),
            specs = rt.agent.tools.list() => specs,
        };
        for note in rt.agent.tools.list_errors() {
            report.diagnostics.push(Diagnostic { code: DiagnosticCode::ToolInventory, source: "tools".into(), message: note });
        }
        let missing: Vec<_> = request.capabilities.required_tools.iter()
            .filter(|name| !specs.iter().any(|spec| &spec.name == *name)).cloned().collect();
        for name in &missing {
            report.diagnostics.push(Diagnostic { code: DiagnosticCode::RequiredToolMissing,
                source: name.clone(), message: format!("required tool `{name}` is unavailable or excluded") });
        }
        if !missing.is_empty() { anyhow::bail!("required tools unavailable: {}", missing.join(", ")); }
        let task = match input {
            Some(task) => task,
            None => {
                let TaskInput::Command { name, args } = &request.input else { unreachable!() };
                let template = rt.task_templates.iter().find(|t| &t.name == name)
                    .with_context(|| format!("unknown task template `{name}`; use plugin:name from an enabled plugin"))?;
                crate::commands::expand_bounded(template, args)?
            }
        };
        if task.trim().is_empty() { anyhow::bail!("task must not be empty or whitespace"); }
        let mut session = match resumed.take() {
            Some(session) => session,
            None => Session::create(&cwd, &rt.provider_name, &rt.model)?,
        };
        report.session_id = Some(session.header.id.clone());
        event::emit(&events, Event::SessionStarted { id: session.header.id.clone() }).await;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {},
            result = rt.agent.run_session_event(HookEvent::SessionStart, &session) => {
                if let Some(note) = session_hook_note(HookEvent::SessionStart, result) { report.diagnostics.push(note); }
            }
        }
        let result = rt.agent.run_turn(&mut session, task, cancel.clone(), events).await;
        if matches!(&result, Ok(TurnResult::Done)) {
            if let Some(message) = session.messages.last().filter(|m| m.role == Role::Assistant) {
                report.output = message.content.iter().filter_map(|c| match c { Content::Text(t) => Some(t.as_str()), _ => None }).collect::<Vec<_>>().join("");
                report.usage = message.usage;
            }
        }
        let end = rt.agent.run_session_event(HookEvent::SessionEnd, &session);
        tokio::pin!(end);
        let end_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => tokio::time::timeout(Duration::from_secs(2), &mut end).await
                .map_err(anyhow::Error::from).and_then(|result| result),
            result = &mut end => result,
        };
        if let Some(note) = session_hook_note(HookEvent::SessionEnd, end_result) { report.diagnostics.push(note); }
        result
    }.await;
    if tokio::time::timeout(Duration::from_secs(3), rt.agent.tools.shutdown())
        .await
        .is_err()
    {
        report.diagnostics.push(Diagnostic {
            code: DiagnosticCode::ToolShutdown,
            source: "tools".into(),
            message: "tool shutdown timed out".into(),
        });
    }
    result
}

fn read_task(input: &TaskInput) -> crate::Result<Option<String>> {
    const MAX_TASK_BYTES: u64 = 1024 * 1024;
    let task = match input {
        TaskInput::Command { .. } => return Ok(None),
        TaskInput::Text(text) => {
            if text.len() as u64 > MAX_TASK_BYTES {
                anyhow::bail!("task exceeds the 1 MiB input limit");
            }
            text.clone()
        }
        TaskInput::File(path) => {
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
            text
        }
    };
    if task.trim().is_empty() {
        anyhow::bail!("task must not be empty or whitespace");
    }
    if task.len() as u64 > MAX_TASK_BYTES {
        anyhow::bail!("task exceeds the 1 MiB input limit");
    }
    Ok(Some(task))
}

fn session_hook_note(event: HookEvent, result: crate::Result<HookDecision>) -> Option<Diagnostic> {
    let message = match result {
        Ok(HookDecision::Allow) => return None,
        Ok(decision) => format!("warning: {event} hook returned {decision:?} on a non-blockable event; ignored (fail-open)"),
        Err(err) => format!("warning: {event} hook failed: {err:#}"),
    };
    Some(Diagnostic {
        code: DiagnosticCode::SessionHook,
        source: event.to_string(),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_task_budget_preserves_text_and_rejects_before_cloning() {
        let task = format!(" {} ", "é".repeat((1024 * 1024 - 2) / 2));
        assert_eq!(
            read_task(&TaskInput::Text(task.clone())).unwrap(),
            Some(task.clone())
        );
        assert!(read_task(&TaskInput::Text(format!("{task}x")))
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
        assert!(read_task(&TaskInput::Text(" \t\n".into())).is_err());
    }

    #[test]
    fn lifecycle_hook_diagnostics_preserve_source_and_do_not_block() {
        assert!(session_hook_note(HookEvent::SessionStart, Ok(HookDecision::Allow)).is_none());
        let error = anyhow::anyhow!("failed to spawn").context("plugin broken");
        let note = session_hook_note(HookEvent::SessionStart, Err(error)).unwrap();
        assert_eq!(note.code, DiagnosticCode::SessionHook);
        assert_eq!(note.source, "SessionStart");
        assert!(note.message.contains("plugin broken: failed to spawn"));
        let note = session_hook_note(
            HookEvent::SessionEnd,
            Ok(HookDecision::Block("policy".into())),
        )
        .unwrap();
        assert!(note.message.contains("ignored (fail-open)"));
    }

    #[tokio::test]
    async fn invalid_deadline_and_precancellation_need_no_environment() {
        let (tx, _rx) = mpsc::channel(1);
        let mut request = RunRequest::new(TaskInput::File(PathBuf::from("does-not-exist")));
        request.timeout_secs = 0;
        assert_eq!(
            run(request.clone(), CancellationToken::new(), tx.clone())
                .await
                .status,
            RunStatus::Failed
        );
        request.timeout_secs = 600;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let report = run(request, cancel, tx).await;
        assert_eq!(report.status, RunStatus::Cancelled);
        assert!(report.session_id.is_none());
    }
}
