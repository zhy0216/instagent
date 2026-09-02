//! shell 工具（第二版 §2.4）。
//!
//! `$SHELL -c` 或 `bash -c`，cwd 为会话目录，用 `03` 的进程组 + kill_on_drop；
//! 超时或取消时 drop [`ProcessGroupChild`] kill 整组（含孙进程）。
//!
//! 输出截断与 drain 超时逻辑参考 goose `developer/shell.rs`（commit `4ad43df`）
//! 的 `render_output` / `truncate_output` / `save_full_output` /
//! `unix_shell` / `OUTPUT_DRAIN_TIMEOUT_MILLIS` 组，按本仓库形态精简搬运
//! （预览取**前** 50 行 / 10KB，goose 取尾部；全文落临时文件返回路径）。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::subprocess::ProcessGroupChild;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// 默认超时（第二版 §2.4）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// 每流上限：2000 行 / 50KB。
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
/// 超出时给前 50 行 / 10KB 预览，全文存临时文件并返回路径。
pub const PREVIEW_LINES: usize = 50;
pub const PREVIEW_BYTES: usize = 10 * 1024;

/// 进程退出后给输出管道最多 500ms 收尾（后台进程占着管道时不等死）。
/// 参考 goose developer/shell.rs:629 `OUTPUT_DRAIN_TIMEOUT_MILLIS`。
const OUTPUT_DRAIN_TIMEOUT_MILLIS: u64 = 500;

/// 把管道一路读进共享缓冲（截断在渲染阶段做，全文要能落盘）。
async fn pump<R>(mut reader: R, buffer: Arc<Mutex<String>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buffer
                    .lock()
                    .await
                    .push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
            Err(_) => break,
        }
    }
}

/// 等一个 pump 任务收尾，超时则 abort 并标记截断。参考 goose 的 drain 分支。
async fn finish_pump(task: &mut JoinHandle<()>, buffer: &Arc<Mutex<String>>) -> (String, bool) {
    let timed_out = tokio::time::timeout(
        Duration::from_millis(OUTPUT_DRAIN_TIMEOUT_MILLIS),
        &mut *task,
    )
    .await
    .is_err();
    if timed_out {
        task.abort();
        let _ = task.await;
    }
    let text = buffer.lock().await.clone();
    (text, timed_out)
}

/// 非 POSIX 登录 shell（fish/csh/tcsh/nu）会吞掉 heredoc、`$VAR`、`2>&1` 等
/// POSIX 语法，所以不自动采用 `$SHELL`：优先 bash，其次才退回 `$SHELL`。
/// 参考 goose developer/shell.rs `unix_shell`（GOOSE_SHELL 改名为 INSTAGENT_SHELL）。
fn resolve_shell(configured: Option<&Path>) -> PathBuf {
    if let Some(shell) = configured {
        return shell.to_path_buf();
    }
    if let Ok(shell) = std::env::var("INSTAGENT_SHELL") {
        return PathBuf::from(shell);
    }
    if let Some(bash) = find_in_path("bash") {
        return bash;
    }
    match std::env::var("SHELL") {
        Ok(shell) if !shell.is_empty() => PathBuf::from(shell),
        _ => PathBuf::from("sh"),
    }
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file().then_some(candidate)
    })
}

enum Outcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

/// 跑一条命令，返回 stdout、stderr、exit code 的组合格式（is_error = 非零退出码、
/// 超时或被取消）。
pub async fn run(
    command: &str,
    timeout_secs: Option<u64>,
    shell: Option<&Path>,
    ctx: &ToolCtx,
) -> ToolOutput {
    let shell = resolve_shell(shell);
    let mut cmd = Command::new(&shell);
    cmd.arg("-c").arg(command);
    cmd.current_dir(&ctx.cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match ProcessGroupChild::spawn(&mut cmd) {
        Ok(child) => child,
        Err(e) => {
            return ToolOutput::err(format!(
                "Failed to spawn `{} -c {command}`: {e}",
                shell.display()
            ));
        }
    };

    let stdout_pipe = child.child_mut().stdout.take().expect("stdout piped");
    let stderr_pipe = child.child_mut().stderr.take().expect("stderr piped");

    let stdout_buffer = Arc::new(Mutex::new(String::new()));
    let stderr_buffer = Arc::new(Mutex::new(String::new()));
    let mut stdout_task = tokio::spawn(pump(stdout_pipe, Arc::clone(&stdout_buffer)));
    let mut stderr_task = tokio::spawn(pump(stderr_pipe, Arc::clone(&stderr_buffer)));

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let outcome = {
        let wait = child.child_mut().wait();
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            status = wait => Outcome::Exited(status.ok().and_then(|s| s.code())),
        }
    };
    // 超时/取消：drop ProcessGroupChild 对整组 SIGKILL（03 的进程组守卫）。
    if !matches!(outcome, Outcome::Exited(_)) {
        drop(child);
    }

    let (stdout_raw, stdout_drain_cut) = finish_pump(&mut stdout_task, &stdout_buffer).await;
    let (stderr_raw, stderr_drain_cut) = finish_pump(&mut stderr_task, &stderr_buffer).await;

    let stdout = truncate_stream(&stdout_raw, "stdout");
    let stderr = truncate_stream(&stderr_raw, "stderr");
    let timed_out = matches!(outcome, Outcome::TimedOut);
    let cancelled = matches!(outcome, Outcome::Cancelled);
    let exit_code = match outcome {
        Outcome::Exited(code) => code,
        _ => None,
    };

    let mut text = render_output(&stdout, &stderr, exit_code);
    if timed_out {
        text.push_str(&format!(
            "note: command timed out after {}s; the whole process group was killed.\n",
            timeout.as_secs()
        ));
    }
    if cancelled {
        text.push_str("note: command cancelled; the whole process group was killed.\n");
    }
    if stdout_drain_cut || stderr_drain_cut {
        text.push_str(
            "note: output collection was cut short after the shell exited \
             (backgrounded process?).\n",
        );
    }

    let failed = timed_out || cancelled || !matches!(exit_code, Some(0));
    if failed {
        ToolOutput::err(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// 单流截断：≤ 2000 行且 ≤ 50KB 原样返回；否则存全文到临时文件，
/// 返回前 50 行 / 10KB 预览 + 说明。参考 goose developer/shell.rs
/// `truncate_output` / `truncation_notice`（预览取头不取尾）。
fn truncate_stream(text: &str, label: &str) -> String {
    if text.is_empty() {
        return "(no output)".to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let exceeded_lines = lines.len() > MAX_LINES;
    let exceeded_bytes = text.len() > MAX_BYTES;
    if !exceeded_lines && !exceeded_bytes {
        return text.to_string();
    }

    let reason = if exceeded_lines {
        format!(
            "Output exceeded {MAX_LINES} line limit ({} lines total).",
            lines.len()
        )
    } else {
        format!(
            "Output exceeded {MAX_BYTES} byte limit ({} bytes total).",
            text.len()
        )
    };
    let saved_note = match save_full_output(text, label) {
        Ok(path) => format!(
            " Full output saved to {}. Read it with shell commands like head, tail, or \
             sed -n '100,200p' up to {MAX_LINES} lines at a time.",
            path.display()
        ),
        Err(e) => format!(" (failed to save full output: {e})"),
    };

    let preview = truncate_preview_bytes(lines[..PREVIEW_LINES.min(lines.len())].join("\n"));
    format!("[{reason}{saved_note}]\n{preview}")
}

/// 预览再按字节封顶，防止无换行的巨行（进度条 / base64）绕过行截断。
/// 参考 goose developer/shell.rs `truncate_preview_bytes`。
#[allow(clippy::string_slice)] // end 已经吸附到 char boundary。
fn truncate_preview_bytes(preview: String) -> String {
    if preview.len() <= PREVIEW_BYTES {
        return preview;
    }
    let mut end = PREVIEW_BYTES;
    while !preview.is_char_boundary(end) {
        end -= 1;
    }
    preview[..end].to_string()
}

/// 全文落临时文件并返回路径。参考 goose developer/shell.rs `save_full_output`
/// （goose 按 label 复用固定路径，这里加 uuid 防并发覆盖）。
fn save_full_output(output: &str, label: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("instagent-shell-output");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{label}-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, output)?;
    Ok(path)
}

/// 拼装最终文本（截断后的 stdout/stderr 段 + exit code）。
pub fn render_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> String {
    let mut text = String::new();
    text.push_str("stdout:\n");
    text.push_str(stdout);
    if !stdout.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("stderr:\n");
    text.push_str(stderr);
    if !stderr.ends_with('\n') {
        text.push('\n');
    }
    match exit_code {
        Some(code) => text.push_str(&format!("exit code: {code}\n")),
        None => text.push_str("exit code: unknown (process was killed)\n"),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path, cancel: CancellationToken) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel,
        }
    }

    #[cfg(unix)]
    async fn signalable(pid: u32) -> bool {
        tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }

    #[cfg(unix)]
    async fn eventually_dead(pid: u32) {
        for _ in 0..100 {
            if !signalable(pid).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pid {pid} still alive after kill");
    }

    #[cfg(unix)]
    fn pid_of(pid_file: &Path) -> u32 {
        std::fs::read_to_string(pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    #[tokio::test]
    async fn shell_runs_command_with_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("echo hi && echo oops >&2", None, None, &ctx).await;
        assert!(!out.is_error);
        assert!(out.text.contains("hi"));
        assert!(out.text.contains("oops"));
        assert!(out.text.contains("exit code: 0"));
    }

    #[tokio::test]
    async fn shell_nonzero_exit_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("exit 3", None, None, &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn shell_cwd_is_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("pwd", None, None, &ctx).await;
        assert!(
            out.text.contains(dir.path().to_str().unwrap()),
            "pwd should report session cwd: {}",
            out.text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_kills_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let ctx = ctx(dir.path(), CancellationToken::new());
        let command = format!("sleep 120 & echo $! > {}; sleep 120", pid_file.display());

        let started = std::time::Instant::now();
        let out = run(&command, Some(1), None, &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));

        eventually_dead(pid_of(&pid_file)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancel_kills_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let token = CancellationToken::new();
        let ctx = ctx(dir.path(), token.clone());
        let command = format!("sleep 120 & echo $! > {}; sleep 120", pid_file.display());

        let handle = tokio::spawn(async move { run(&command, Some(300), None, &ctx).await });

        let mut seen = false;
        for _ in 0..100 {
            if pid_file.exists() {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(seen, "grandchild pid never recorded");
        token.cancel();

        let out = handle.await.expect("shell task");
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"));
        eventually_dead(pid_of(&pid_file)).await;
    }

    #[tokio::test]
    async fn shell_truncates_and_saves_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("seq 1 2500", None, None, &ctx).await;
        assert!(!out.is_error);
        assert!(out.text.contains("exceeded 2000 line limit"));
        assert!(out.text.contains("Full output saved to"));

        let marker = "Full output saved to ";
        let path = out
            .text
            .lines()
            .find_map(|line| {
                line.find(marker).map(|i| {
                    let rest = &line[i + marker.len()..];
                    rest.split(' ')
                        .next()
                        .unwrap()
                        .trim_end_matches('.')
                        .to_string()
                })
            })
            .expect("saved path in output");
        let full = std::fs::read_to_string(&path).expect("saved full output");
        let full_lines: Vec<&str> = full.lines().collect();
        assert_eq!(full_lines.len(), 2500);
        assert_eq!(full_lines[0], "1");
        assert_eq!(full_lines[2499], "2500");
        // 预览只带头 50 行。
        assert!(out.text.contains("\n50\n"));
        assert!(!out.text.contains("2499"));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn shell_empty_output_shows_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("true", None, None, &ctx).await;
        assert_eq!(out.text.matches("(no output)").count(), 2);
    }

    #[tokio::test]
    async fn shell_respects_configured_shell() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        let out = run("echo ${0##*/}-ok", None, Some(Path::new("/bin/sh")), &ctx).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("sh-ok"));
    }

    #[test]
    fn render_output_format() {
        let text = render_output("hello", "", Some(0));
        assert!(text.starts_with("stdout:\nhello\nstderr:\n"));
        assert!(text.contains("exit code: 0"));
        let text = render_output("a", "b", None);
        assert!(text.contains("exit code: unknown"));
    }

    #[test]
    fn preview_is_byte_bounded_for_giant_line() {
        let giant = "x".repeat(MAX_BYTES + 10);
        let out = truncate_stream(&giant, "stdout");
        assert!(out.contains("byte limit"));
        let preview = out.split_once('\n').unwrap().1;
        assert!(preview.len() <= PREVIEW_BYTES);
    }

    #[test]
    fn truncate_under_limits_passes_through() {
        let text = "small\noutput";
        assert_eq!(truncate_stream(text, "stdout"), text);
        assert_eq!(truncate_stream("", "stdout"), "(no output)");
    }
}
