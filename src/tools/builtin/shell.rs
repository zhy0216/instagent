//! shell 工具（第二版 §2.4）。
//!
//! `$SHELL -c` 或 `bash -c`，cwd 为会话目录，用 `03` 的进程组 + kill_on_drop；
//! 输出走 [`run_bounded`] 有界收集（每路 [`COLLECT_CAP_BYTES`] 硬上限）：
//! 超限 / 超时 / 取消时 drop [`ProcessGroupChild`] kill 整组（含孙进程），
//! 并返回可操作摘要（截断标记、原因、杀组说明）。builtin shell 是 ADR 0003 D2
//! 的唯一例外：保留父进程完整环境（它是模型操作 sandbox 用户 shell 的通道）。
//!
//! 输出截断与 drain 超时逻辑参考 goose `developer/shell.rs`（commit `4ad43df`）
//! 的 `render_output` / `truncate_output` / `save_full_output` /
//! `unix_shell` / `OUTPUT_DRAIN_TIMEOUT_MILLIS` 组，按本仓库形态精简搬运
//! （预览取**前** 50 行 / 10KB，goose 取尾部；全文落临时文件返回路径）。
//!
//! 超限落盘的 spill 文件按 `会话标记/流标签-随机uuid` 命名（R11 / todo 06）：
//! 目录与文件私有权限，每次新保存前做 TTL 清理，清理失败有可见诊断。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::process::Command;

use crate::subprocess::run_bounded;
use crate::subprocess::Outcome;
use crate::subprocess::ProcessGroupChild;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// 默认超时（第二版 §2.4）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// 展示上限：2000 行 / 50KB，超出给预览 + 全文落临时文件。
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
/// 超出时给前 50 行 / 10KB 预览，全文存临时文件并返回路径。
pub const PREVIEW_LINES: usize = 50;
pub const PREVIEW_BYTES: usize = 10 * 1024;
/// [`run_bounded`] 每路收集硬上限（1 MiB）：比展示上限高一个量级，给
/// spill 文件留余地；超限杀整个进程组、保留截断头部与摘要（R3 / todo 06）。
pub const COLLECT_CAP_BYTES: usize = 1024 * 1024;
/// spill 文件保留期：每次新保存前清理过期文件（惰性、有界），
/// 敏感输出不长期留在磁盘（R11）。
pub const SPILL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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

    let child = match ProcessGroupChild::spawn(&mut cmd) {
        Ok(child) => child,
        Err(e) => {
            return ToolOutput::err(format!(
                "Failed to spawn `{} -c {command}`: {e}",
                shell.display()
            ));
        }
    };

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let run = match run_bounded(child, COLLECT_CAP_BYTES, timeout, Some(&ctx.cancel)).await {
        Ok(run) => run,
        Err(e) => return ToolOutput::err(format!("subprocess configuration error: {e}")),
    };

    let stdout = truncate_stream(&run.stdout.text, "stdout");
    let stderr = truncate_stream(&run.stderr.text, "stderr");
    let timed_out = run.outcome == Outcome::TimedOut;
    let cancelled = run.outcome == Outcome::Cancelled;
    let overflowed = run.stdout.truncated || run.stderr.truncated;
    let exit_code = match run.outcome {
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
    if overflowed {
        text.push_str(&format!(
            "note: output exceeded the {COLLECT_CAP_BYTES} byte collection cap; \
             the whole process group was killed and only the kept head is shown.\n"
        ));
    }
    if run.drained_short {
        text.push_str(
            "note: output collection was cut short after the shell exited \
             (backgrounded process?).\n",
        );
    }

    let failed = timed_out || cancelled || overflowed || !matches!(exit_code, Some(0));
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

fn spill_root() -> PathBuf {
    std::env::temp_dir().join("instagent-shell-output")
}

/// 本进程的会话标记：CLI 一个进程跑一个会话，随机 id 足以把不同会话的
/// spill 文件分开（R11 的 session 后缀；文件级再加随机 uuid 防并发覆盖）。
fn session_marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

/// 全文落临时文件并返回路径：`<root>/<会话标记>/<label>-<uuid>`。
/// 每次写入前做惰性 TTL 清理。参考 goose developer/shell.rs
/// `save_full_output`（goose 按 label 复用固定路径，这里加会话/随机后缀）。
fn save_full_output(output: &str, label: &str) -> std::io::Result<PathBuf> {
    let root = spill_root();
    cleanup_expired_spills(&root, SPILL_TTL, session_marker());
    let dir = root.join(session_marker());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{label}-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 全量输出可能含敏感信息：目录 0700、文件 0600，同机其他用户不可读。
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// TTL 清理（R11）：删过期 spill 文件与空的会话目录（`keep_session` 是
/// 当前会话目录，不删）。每次保存时惰性触发，兜底"消费 / 会话结束"场景
/// （工具层挂不到这两个时机，靠 TTL 保证不留长期敏感文件）；清理失败只
/// warn、不打断保存。
fn cleanup_expired_spills(root: &Path, ttl: Duration, keep_session: &str) {
    let now = std::time::SystemTime::now();
    let Ok(sessions) = std::fs::read_dir(root) else {
        return; // 首次保存：根目录尚不存在。
    };
    for session in sessions.flatten() {
        let dir = session.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(&dir) {
            for file in files.flatten() {
                let expired = file
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| now.duration_since(mtime).ok())
                    .is_some_and(|age| age > ttl);
                if expired {
                    if let Err(err) = std::fs::remove_file(file.path()) {
                        tracing::warn!(
                            "清理过期 shell 输出文件失败 {}: {err}",
                            file.path().display()
                        );
                    }
                }
            }
        }
        let is_current = dir.file_name().and_then(|n| n.to_str()) == Some(keep_session);
        if !is_current && std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none())
        {
            if let Err(err) = std::fs::remove_dir(&dir) {
                tracing::warn!("清理空 shell 输出目录失败 {}: {err}", dir.display());
            }
        }
    }
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

    /// pid 落盘的有界轮询（10ms 一次、最多 0.5s）。只在被测任务返回
    /// （组已 SIGKILL）之后调用：文件此时不再变化——有则进程启动过，
    /// 无则说明它直到超时都没启动，组杀检查没有可观测对象。
    #[cfg(unix)]
    async fn wait_pid_file(path: &Path) -> Option<u32> {
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    if pid > 0 {
                        return Some(pid);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
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
    #[test]
    fn full_output_file_is_private_and_session_scoped() {
        use std::os::unix::fs::PermissionsExt;
        let path = save_full_output("secret output", "perm-test").unwrap();
        let dir = path.parent().unwrap();
        let root = dir.parent().unwrap();
        assert_eq!(root, spill_root(), "落在统一 spill 根下");
        assert_eq!(
            dir.file_name().unwrap().to_str().unwrap(),
            session_marker(),
            "会话后缀目录（R11）"
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("perm-test-"),
            "label + 随机 uuid 后缀"
        );
        let root_mode = std::fs::metadata(root).unwrap().permissions().mode();
        let dir_mode = std::fs::metadata(dir).unwrap().permissions().mode();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let _ = std::fs::remove_file(&path);
        assert_eq!(root_mode & 0o777, 0o700, "root perms: {root_mode:#o}");
        assert_eq!(dir_mode & 0o777, 0o700, "dir perms: {dir_mode:#o}");
        assert_eq!(file_mode & 0o777, 0o600, "file perms: {file_mode:#o}");
    }

    #[test]
    fn spill_cleanup_removes_expired_files_and_empty_session_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("spill");
        let old_session = root.join("sess-old");
        let keep = root.join("keep");
        std::fs::create_dir_all(&old_session).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        let expired = old_session.join("stdout-xyz");
        std::fs::write(&expired, "old").unwrap();
        // mtime 落到过去，避免零龄边界。
        std::thread::sleep(Duration::from_millis(20));

        cleanup_expired_spills(&root, Duration::ZERO, "keep");
        assert!(!expired.exists(), "TTL 过期文件应被清理");
        assert!(!old_session.exists(), "清空的会话目录应被移除");
        assert!(keep.exists(), "当前会话目录不删");

        // 未过期文件保留。
        let fresh = keep.join("stdout-fresh");
        std::fs::write(&fresh, "new").unwrap();
        cleanup_expired_spills(&root, SPILL_TTL, "keep");
        assert!(fresh.exists(), "TTL 内文件保留");
    }

    /// 输出越过收集硬上限：进程组被杀、is_error，带可操作摘要
    /// （可控 fake：/dev/zero 洪泛，只有杀组后才能收敛）。
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_output_over_collect_cap_kills_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), CancellationToken::new());
        // 1.2MB > COLLECT_CAP_BYTES（1MiB）。
        let out = run(
            "head -c 1200000 /dev/zero | tr '\\0' x",
            Some(60),
            None,
            &ctx,
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.text.contains("byte collection cap"),
            "超限要说明原因：{}",
            out.text
        );
        assert!(
            out.text.contains("the whole process group was killed"),
            "{}",
            out.text
        );
        // 保留的头部仍然走展示截断：预览 + 全文落盘说明。
        assert!(out.text.contains("byte limit"), "{}", out.text);
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
        if let Some(pid) = wait_pid_file(&pid_file).await {
            eventually_dead(pid).await;
        }
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
