//! 子进程：一律进程组 + `kill_on_drop(true)`，防 Ctrl-C 后残留（第二版 §2.12）。
//!
//! 从 `~/yyds/goose/crates/goose/src/subprocess.rs`（commit `4ad43df`，144 行）移植
//! `configure_subprocess` / `spawn_long_lived_mcp_subprocess` / `git_command`，
//! 含 Linux PR_SET_PDEATHSIG 特判与专用 spawner 线程；因本仓库不新增依赖，
//! libc 符号改为最小 `extern "C"` 声明，并删去 Windows 分支（目标平台 macOS/Linux）。
//! 另加 `ProcessGroupChild` 投递守卫：drop 时 SIGKILL 整个进程组（含孙进程），
//! 弥补 tokio / rmcp 只杀直接子进程的缺口。shell / MCP stdio / hooks / proxy 都复用这里。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use rmcp::transport::TokioChildProcess;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStderr;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use std::sync::{mpsc, OnceLock};

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(target_os = "linux")]
extern "C" {
    fn prctl(option: i32, ...) -> i32;
    fn getpid() -> i32;
    fn getppid() -> i32;
}

#[cfg(target_os = "linux")]
fn configure_parent_death_signal(command: &mut Command) {
    const PR_SET_PDEATHSIG: i32 = 1;
    const SIGTERM: i32 = 15;
    const ESRCH: i32 = 3;

    let parent_pid = unsafe { getpid() };

    unsafe {
        command.pre_exec(move || {
            if prctl(PR_SET_PDEATHSIG, SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }

            if getppid() != parent_pid {
                return Err(io::Error::from_raw_os_error(ESRCH));
            }

            Ok(())
        });
    }
}

fn configure_common_subprocess(command: &mut Command) {
    // Isolate subprocess into its own process group so it does not receive
    // SIGINT when the user presses Ctrl+C in the terminal.
    #[cfg(unix)]
    command.process_group(0);
    // 第二版 §2.12：子进程一律 kill_on_drop(true)，防止句柄泄漏后孤儿进程。
    command.kill_on_drop(true);
}

/// 进程组隔离 + `kill_on_drop(true)` +（Linux）parent-death 信号。
pub fn configure_subprocess(command: &mut Command) {
    configure_common_subprocess(command);
    #[cfg(target_os = "linux")]
    configure_parent_death_signal(command);
}

/// `tokio::process::Child` 的进程组守卫：子进程是自身进程组组长
/// （`configure_subprocess` 保证），drop 时对整组发 SIGKILL，
/// 孙进程一并清掉，不留残留。直接子进程的回收仍由 tokio orphan queue 负责。
#[derive(Debug)]
pub struct ProcessGroupChild {
    child: tokio::process::Child,
}

impl ProcessGroupChild {
    /// `configure_subprocess` 后 spawn，返回带进程组清理语义的句柄。
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        configure_subprocess(command);
        Ok(Self {
            child: command.spawn()?,
        })
    }

    /// 子进程（进程组组长）的 PID。
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// 逃逸舱：直接操作底层 `Child`（wait、kill、stdout/stderr 等）。
    pub fn child_mut(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }
}

impl Drop for ProcessGroupChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // 负 PID = 目标进程组；组内所有成员（含孙进程）一起 SIGKILL。
            unsafe {
                kill(-(pid as i32), SIGKILL);
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct LongLivedSpawnRequest {
    command: Command,
    runtime: tokio::runtime::Handle,
    response: tokio::sync::oneshot::Sender<io::Result<(TokioChildProcess, Option<ChildStderr>)>>,
}

#[cfg(target_os = "linux")]
fn long_lived_spawn_sender() -> io::Result<mpsc::Sender<LongLivedSpawnRequest>> {
    static SENDER: OnceLock<io::Result<mpsc::Sender<LongLivedSpawnRequest>>> = OnceLock::new();

    match SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<LongLivedSpawnRequest>();
        std::thread::Builder::new()
            .name("instagent-extension-spawner".to_owned())
            .spawn(move || {
                while let Ok(mut request) = receiver.recv() {
                    let _runtime_guard = request.runtime.enter();
                    configure_subprocess(&mut request.command);
                    let result = TokioChildProcess::builder(request.command)
                        .stderr(std::process::Stdio::piped())
                        .spawn();
                    let _ = request.response.send(result);
                }
            })
            .map(|_| sender)
    }) {
        Ok(sender) => Ok(sender.clone()),
        Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
    }
}

/// 长驻 MCP stdio 子进程：返回 transport 与被管道化的 stderr（stderr 接日志）。
/// Linux 上把 spawn 挪到专用线程，使 PR_SET_PDEATHSIG 绑定的父线程不会随
/// 某个 Tokio worker 退出而误杀长驻进程（移植自 goose）。
pub async fn spawn_long_lived_mcp_subprocess(
    command: Command,
) -> io::Result<(TokioChildProcess, Option<ChildStderr>)> {
    #[cfg(target_os = "linux")]
    {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        long_lived_spawn_sender()?
            .send(LongLivedSpawnRequest {
                command,
                runtime,
                response: response_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "extension spawner exited"))?;
        response_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "extension spawner exited"))?
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut command = command;
        configure_subprocess(&mut command);
        TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::piped())
            .spawn()
    }
}

/// 加固过的 git 命令（拒绝隐式 bare repo、禁 fsmonitor hook），`07` 安装用。
pub fn git_command() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command.args([
        "-c",
        "safe.bareRepository=explicit",
        "-c",
        "core.fsmonitor=false",
    ]);
    command
}

/// 进程退出 / 被杀后给输出管道最多 500ms 收尾（后台进程占着管道时不等死）。
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// 等待结果（超时 / 取消都不带退出码）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

/// [`wait_and_drain`] 的结果：结局 + 两路全量输出 + 收尾截断标记。
pub(crate) struct RunOutput {
    pub outcome: Outcome,
    pub stdout: String,
    pub stderr: String,
    /// 退出后某路输出读取被 drain 超时切断（后台进程占管道）。
    pub drained_short: bool,
}

impl RunOutput {
    pub fn exit_code(&self) -> Option<i32> {
        match self.outcome {
            Outcome::Exited(code) => code,
            _ => None,
        }
    }
}

/// 载荷后台写 stdin（防大输入死锁；脚本不读时 BrokenPipe 忽略）。
pub(crate) fn write_stdin(child: &mut ProcessGroupChild, payload: &[u8]) {
    if let Some(mut stdin) = child.child_mut().stdin.take() {
        let bytes = payload.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
}

/// 把管道一路读进共享缓冲（截断在渲染阶段做，全文要能落盘；
/// drain 超时时 partial 输出仍可读）。
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

/// 等一个 pump 任务收尾，超时则 abort 并返回已收集的 partial + 截断标记。
async fn finish_pump(task: &mut JoinHandle<()>, buffer: &Arc<Mutex<String>>) -> (String, bool) {
    let timed_out = tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut *task)
        .await
        .is_err();
    if timed_out {
        task.abort();
        let _ = task.await;
    }
    let text = buffer.lock().await.clone();
    (text, timed_out)
}

/// 跑子进程的统一编排：两路管道 pump 输出 → 带取消/超时的等待 →
/// 非正常退出 drop [`ProcessGroupChild`] SIGKILL 整组 → 限时收尾两路输出。
/// `cancel` 为 `None` 时不监听取消。
pub(crate) async fn wait_and_drain(
    mut child: ProcessGroupChild,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> RunOutput {
    let stdout_buffer = Arc::new(Mutex::new(String::new()));
    let stderr_buffer = Arc::new(Mutex::new(String::new()));
    let mut stdout_task = tokio::spawn(pump(
        child.child_mut().stdout.take().expect("stdout piped"),
        Arc::clone(&stdout_buffer),
    ));
    let mut stderr_task = tokio::spawn(pump(
        child.child_mut().stderr.take().expect("stderr piped"),
        Arc::clone(&stderr_buffer),
    ));

    let token = cancel.cloned().unwrap_or_default();
    let outcome = {
        let wait = child.child_mut().wait();
        tokio::select! {
            biased;
            _ = token.cancelled(), if cancel.is_some() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            status = wait => Outcome::Exited(status.ok().and_then(|s| s.code())),
        }
    };
    // 超时/取消：drop ProcessGroupChild 对整组 SIGKILL（03 的进程组守卫）。
    if !matches!(outcome, Outcome::Exited(_)) {
        drop(child);
    }

    let (stdout, stdout_cut) = finish_pump(&mut stdout_task, &stdout_buffer).await;
    let (stderr, stderr_cut) = finish_pump(&mut stderr_task, &stderr_buffer).await;
    RunOutput {
        outcome,
        stdout,
        stderr,
        drained_short: stdout_cut || stderr_cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::time::sleep;
    use tokio::time::Duration;

    #[cfg(unix)]
    fn signalable(pid: i32) -> bool {
        unsafe { kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    async fn eventually(label: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {label}");
    }

    #[cfg(unix)]
    fn read_pid_file(path: &Path) -> Option<i32> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| content.trim().parse::<i32>().ok())
    }

    #[test]
    fn configure_subprocess_enables_kill_on_drop() {
        let mut command = Command::new("true");
        configure_subprocess(&mut command);
        assert!(command.get_kill_on_drop());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_kills_whole_process_group_including_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let grandchild_pid_file = dir.path().join("grandchild.pid");

        // 直接在 `sh` 里起一个后台 `sleep`，孙进程与 `sh` 同属一个进程组。
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sleep 120 & echo $! > {}; wait",
            grandchild_pid_file.display()
        ));

        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let pgid = child.id().expect("child pid") as i32;

        let mut grandchild: Option<i32> = None;
        eventually("grandchild pid recorded", || {
            grandchild = read_pid_file(&grandchild_pid_file);
            grandchild.is_some()
        })
        .await;
        let gpid = grandchild.unwrap();

        assert!(signalable(pgid), "direct child should be alive");
        assert!(signalable(gpid), "grandchild should be alive");
        assert!(
            signalable(-pgid),
            "child should lead its own process group (process_group(0))"
        );

        drop(child);

        eventually("direct child gone", || !signalable(pgid)).await;
        eventually("grandchild gone", || !signalable(gpid)).await;
        eventually("process group gone", || !signalable(-pgid)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mcp_subprocess_is_killed_on_drop() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 120");

        let (child, _stderr) = spawn_long_lived_mcp_subprocess(command)
            .await
            .expect("spawn mcp subprocess");
        let pid = child.id().expect("child pid") as i32;
        eventually("mcp subprocess alive", || signalable(pid)).await;

        drop(child);

        eventually("mcp subprocess gone", || !signalable(pid)).await;
    }

    #[tokio::test]
    async fn wait_and_drain_collects_output_and_exit_code() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("echo out; echo err >&2; exit 4")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let out = wait_and_drain(child, Duration::from_secs(10), None).await;
        assert_eq!(out.outcome, Outcome::Exited(Some(4)));
        assert_eq!(out.exit_code(), Some(4));
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.drained_short);
    }

    #[tokio::test]
    async fn wait_and_drain_timeout_kills_and_reports() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 120")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let out = wait_and_drain(child, Duration::from_millis(50), None).await;
        assert_eq!(out.outcome, Outcome::TimedOut);
        assert_eq!(out.exit_code(), None);
    }

    #[test]
    fn git_command_hardens_repository_and_fsmonitor() {
        let args: Vec<String> = git_command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "safe.bareRepository=explicit"));
        assert!(args.iter().any(|a| a == "core.fsmonitor=false"));
    }
}
