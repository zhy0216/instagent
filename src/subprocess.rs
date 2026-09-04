//! 子进程：一律进程组 + `kill_on_drop(true)`，防 Ctrl-C 后残留（第二版 §2.12）。
//!
//! 从 `~/yyds/goose/crates/goose/src/subprocess.rs`（commit `4ad43df`，144 行）移植
//! `configure_subprocess` / `spawn_long_lived_mcp_subprocess` / `git_command`，
//! 含 Linux PR_SET_PDEATHSIG 特判与专用 spawner 线程；因本仓库不新增依赖，
//! libc 符号改为最小 `extern "C"` 声明，并删去 Windows 分支（目标平台 macOS/Linux）。
//! 另加 `ProcessGroupChild` 投递守卫：drop 时 SIGKILL 整个进程组（含孙进程），
//! 弥补 tokio / rmcp 只杀直接子进程的缺口。shell / MCP stdio / hooks / proxy 都复用这里。
//!
//! 输出收集走 [`run_bounded`]：每路硬上限 + 增量 UTF-8 解码，超限即杀整组并保留
//! 截断头部与 [`BoundedOutput::truncated`] 状态；[`wait_and_drain`] 是带默认上限、
//! 压平成字符串的兼容入口。

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

/// [`wait_and_drain`] 兼容入口的每路默认硬上限（1 MiB）。
/// 新调用方（shell / command / hooks / install）应经 [`run_bounded`] 显式传自己的预算。
pub(crate) const DEFAULT_OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// 等待结果（超时 / 取消都不带退出码；输出超限被主动杀组时同样只有
/// `Exited(None)`，区分靠 [`BoundedOutput::truncated`]）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

/// 单路输出的有界收集结果：截断头部 + 超限状态，绝不无界进内存。
#[derive(Debug, Clone)]
pub(crate) struct BoundedOutput {
    /// 保留的头部文本，最多收集时生效的硬上限字节数（吸附到字符边界，无半字符）。
    pub text: String,
    /// 输出总量超过硬上限：多余部分被丢弃，且进程组已被终止。
    pub truncated: bool,
    /// 观测到的解码后总字节数（含超限后被丢弃的部分）。
    pub total_bytes: usize,
}

impl BoundedOutput {
    /// 截断摘要标记；未超限时为 `None`。渲染方（stdout/stderr 标签由调用方持有）
    /// 可直接拼在自己的输出段里。
    pub fn truncation_note(&self) -> Option<String> {
        self.truncated.then(|| {
            format!(
                "[output truncated: kept first {} of {} decoded bytes; the rest was dropped \
                 and the process group killed]",
                self.text.len(),
                self.total_bytes
            )
        })
    }
}

/// [`run_bounded`] 的结果：结局 + 两路独立有界输出 + 收尾截断标记。
/// 两路各自封顶、各自报告超限，合并规则（标签、顺序、渲染）完全由调用方决定。
/// 退出码取 `outcome`；`Exited(None)` 且某路 `truncated` 即为输出越限杀组。
#[derive(Debug)]
pub(crate) struct CollectedRun {
    pub outcome: Outcome,
    pub stdout: BoundedOutput,
    pub stderr: BoundedOutput,
    /// 退出后某路输出读取被 drain 超时切断（后台进程占管道）。
    pub drained_short: bool,
}

/// [`wait_and_drain`] 的结果：结局 + 两路（默认上限内）输出 + 收尾截断标记。
/// 超限文本自带 [`BoundedOutput::truncation_note`] 行内标记。
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

/// 增量 UTF-8 解码器：跨 chunk 缓冲末尾的不完整多字节序列，
/// 避免 chunk 边界把合法字符切成 replacement；真正坏掉的尾部在
/// [`Utf8StreamDecoder::finish`] 时按 lossy 出 replacement。
#[derive(Default)]
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let mut out = String::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            match std::str::from_utf8(&bytes[offset..]) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(err) => {
                    let valid_upto = offset + err.valid_up_to();
                    // [offset, valid_upto) 由 from_utf8 保证合法，lossy 原样透传。
                    out.push_str(&String::from_utf8_lossy(&bytes[offset..valid_upto]));
                    match err.error_len() {
                        // 确定的坏序列：一个 maximal 非法子序列一个 replacement，同 lossy。
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            offset = valid_upto + len;
                        }
                        // 尾部不完整：只把坏尾留给下一个 chunk，已吐出的合法前缀不重复。
                        None => {
                            self.pending.extend_from_slice(&bytes[valid_upto..]);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

/// 单路输出的有界收集状态（pump 任务与编排共享，锁内只碰内存）。
struct StreamCollector {
    max_bytes: usize,
    decoder: Utf8StreamDecoder,
    text: String,
    total_bytes: usize,
    truncated: bool,
}

impl StreamCollector {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            decoder: Utf8StreamDecoder::default(),
            text: String::new(),
            total_bytes: 0,
            truncated: false,
        }
    }

    /// 喂入一段原始字节：增量解码后封顶保留头部。
    /// 首次越过硬上限时返回 `true`，pump 借此通知编排终止进程组。
    fn push(&mut self, chunk: &[u8]) -> bool {
        let text = self.decoder.push(chunk);
        self.absorb(&text)
    }

    /// 流结束：冲刷解码器里残留的不完整尾部。
    fn flush(&mut self) -> bool {
        let tail = self.decoder.finish();
        self.absorb(&tail)
    }

    fn absorb(&mut self, text: &str) -> bool {
        self.total_bytes += text.len();
        if self.text.len() < self.max_bytes {
            let mut end = (self.max_bytes - self.text.len()).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            #[allow(clippy::string_slice)] // end 已吸附到 char boundary。
            {
                self.text.push_str(&text[..end]);
            }
        }
        let limited = self.total_bytes > self.max_bytes;
        limited && !std::mem::replace(&mut self.truncated, limited)
    }

    fn finish(&mut self) -> BoundedOutput {
        BoundedOutput {
            text: std::mem::take(&mut self.text),
            truncated: self.truncated,
            total_bytes: self.total_bytes,
        }
    }
}

/// 把管道一路增量解码进有界收集器；首次越限时取消 `overflow` 通知编排杀组。
/// drain 超时时 partial 输出仍可读。
async fn pump<R>(mut reader: R, collector: Arc<Mutex<StreamCollector>>, overflow: CancellationToken)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut chunk = [0u8; 8192];
    loop {
        let limited = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => collector.lock().await.push(&chunk[..n]),
            Err(_) => break,
        };
        if limited {
            overflow.cancel();
        }
    }
    if collector.lock().await.flush() {
        overflow.cancel();
    }
}

/// 等一个 pump 任务收尾，超时则 abort 并返回已收集的 partial + 截断标记。
async fn finish_pump(
    task: &mut JoinHandle<()>,
    collector: &Arc<Mutex<StreamCollector>>,
) -> (BoundedOutput, bool) {
    let timed_out = tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut *task)
        .await
        .is_err();
    if timed_out {
        task.abort();
        let _ = task.await;
    }
    let output = collector.lock().await.finish();
    (output, timed_out)
}

/// 带硬上限输出收集的统一编排：两路管道增量解码进 collector → 带取消/超时的等待 →
/// 任一路上限越限 / 超时 / 取消 / 非正常退出都 drop [`ProcessGroupChild`] SIGKILL 整组 →
/// 限时收尾两路输出。`cancel` 为 `None` 时不监听取消。
///
/// 未把 stdout/stderr 配成 pipe 时返回带 pid 上下文的错误（不再 panic）；
/// 越限状态看 [`BoundedOutput::truncated`]，摘要看 [`BoundedOutput::truncation_note`]。
pub(crate) async fn run_bounded(
    mut child: ProcessGroupChild,
    max_bytes_per_stream: usize,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> io::Result<CollectedRun> {
    let stdout_pipe = child.child_mut().stdout.take();
    let Some(stdout_pipe) = stdout_pipe else {
        return Err(io::Error::other(format!(
            "subprocess (pid {:?}) was spawned without stdout piped; \
             set `Stdio::piped()` on the Command before spawning",
            child.id()
        )));
    };
    let stderr_pipe = child.child_mut().stderr.take();
    let Some(stderr_pipe) = stderr_pipe else {
        return Err(io::Error::other(format!(
            "subprocess (pid {:?}) was spawned without stderr piped; \
             set `Stdio::piped()` on the Command before spawning",
            child.id()
        )));
    };

    let stdout_collector = Arc::new(Mutex::new(StreamCollector::new(max_bytes_per_stream)));
    let stderr_collector = Arc::new(Mutex::new(StreamCollector::new(max_bytes_per_stream)));
    let overflow = CancellationToken::new();
    let mut stdout_task = tokio::spawn(pump(
        stdout_pipe,
        Arc::clone(&stdout_collector),
        overflow.clone(),
    ));
    let mut stderr_task = tokio::spawn(pump(
        stderr_pipe,
        Arc::clone(&stderr_collector),
        overflow.clone(),
    ));

    let token = cancel.cloned().unwrap_or_default();
    let outcome = {
        let wait = child.child_mut().wait();
        tokio::select! {
            biased;
            _ = token.cancelled(), if cancel.is_some() => Outcome::Cancelled,
            // 越限：按 Exited(None) 收场（组随即被 drop 杀掉），区分靠 truncated。
            _ = overflow.cancelled() => Outcome::Exited(None),
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            status = wait => Outcome::Exited(status.ok().and_then(|s| s.code())),
        }
    };
    // 超时/取消/输出越限：drop ProcessGroupChild 对整组 SIGKILL（03 的进程组守卫）。
    // 必须在收尾 pump 之前杀，否则洪泛进程会让 pump 永远读不到 EOF。
    let mut child = if overflow.is_cancelled() || !matches!(outcome, Outcome::Exited(_)) {
        drop(child);
        None
    } else {
        Some(child)
    };

    let (stdout, stdout_cut) = finish_pump(&mut stdout_task, &stdout_collector).await;
    let (stderr, stderr_cut) = finish_pump(&mut stderr_task, &stderr_collector).await;
    // 竞态：进程先自然退出、pump 收尾时才越限，守卫同样要落地。
    if stdout.truncated || stderr.truncated {
        drop(child.take());
    }
    Ok(CollectedRun {
        outcome,
        stdout,
        stderr,
        drained_short: stdout_cut || stderr_cut,
    })
}

/// 兼容入口：以 [`DEFAULT_OUTPUT_CAP_BYTES`] 走 [`run_bounded`]，把两路
/// [`BoundedOutput`] 压平成字符串（超限时行内追加截断摘要）。
/// 配置错误（未配 pipe）不 panic：压平成 `Exited(None)` + stderr 错误文案。
pub(crate) async fn wait_and_drain(
    child: ProcessGroupChild,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> RunOutput {
    match run_bounded(child, DEFAULT_OUTPUT_CAP_BYTES, timeout, cancel).await {
        Ok(run) => RunOutput {
            outcome: run.outcome,
            stdout: flatten_output(&run.stdout),
            stderr: flatten_output(&run.stderr),
            drained_short: run.drained_short,
        },
        Err(err) => RunOutput {
            outcome: Outcome::Exited(None),
            stdout: String::new(),
            stderr: format!("subprocess configuration error: {err}"),
            drained_short: false,
        },
    }
}

/// 压平单路有界输出：保留头部；越限时在末尾接一行截断摘要。
fn flatten_output(stream: &BoundedOutput) -> String {
    match stream.truncation_note() {
        Some(note) => {
            let mut text = stream.text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&note);
            text.push('\n');
            text
        }
        None => stream.text.clone(),
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

    #[test]
    fn utf8_decoder_reassembles_chunks_cut_at_byte_boundaries() {
        let text = "héllo 日本 🎉x";
        let bytes = text.as_bytes();

        for cut in 0..=bytes.len() {
            let mut decoder = Utf8StreamDecoder::default();
            let mut out = decoder.push(&bytes[..cut]);
            out.push_str(&decoder.push(&bytes[cut..]));
            out.push_str(&decoder.finish());
            assert_eq!(out, text, "cut at byte {cut}");
            assert!(
                !out.contains(char::REPLACEMENT_CHARACTER),
                "cut at byte {cut} produced replacement chars"
            );
        }

        let mut byte_by_byte = Utf8StreamDecoder::default();
        let mut out = String::new();
        for i in 0..bytes.len() {
            out.push_str(&byte_by_byte.push(&bytes[i..i + 1]));
        }
        out.push_str(&byte_by_byte.finish());
        assert_eq!(out, text);
    }

    #[test]
    fn utf8_decoder_matches_lossy_for_invalid_sequences() {
        // 确定的坏序列（0xFF 0xFE）→ 每个 maximal 非法子序列一个 replacement；
        // 尾部截断序列（0xE4 0xB9）在流内保留，finish 时按 lossy 出 replacement。
        let mut decoder = Utf8StreamDecoder::default();
        let mut out = decoder.push(&[0xFF, 0xFE]);
        out.push_str(&decoder.push("aé".as_bytes()));
        out.push_str(&decoder.push(&[0xE4, 0xB9]));
        out.push_str(&decoder.finish());
        assert_eq!(out, "\u{FFFD}\u{FFFD}aé\u{FFFD}");
        assert_eq!(
            out,
            String::from_utf8_lossy(&[0xFFu8, 0xFE, b'a', 0xC3, 0xA9, 0xE4, 0xB9])
        );
    }

    #[test]
    fn collector_caps_retains_head_and_reports_overflow_once() {
        let mut collector = StreamCollector::new(8);
        assert!(!collector.push(b"1234"));
        assert!(collector.push(b"56789")); // 越限：首次返回 true（通知杀组）
        assert!(!collector.push(b"more")); // 已报告过，不重复触发
        let out = collector.finish();
        assert_eq!(out.text, "12345678");
        assert!(out.truncated);
        assert_eq!(out.total_bytes, 4 + 5 + 4);
        let note = out.truncation_note().expect("truncation summary");
        assert!(note.contains("kept first 8 of 13 decoded bytes"), "{note}");
    }

    #[test]
    fn collector_cap_snaps_to_char_boundary() {
        let mut collector = StreamCollector::new(5);
        assert!(collector.push("日本".as_bytes()));
        assert!(!collector.push("本".as_bytes()));
        let out = collector.finish();
        assert_eq!(out.text, "日"); // 第二字要 6 字节 > 5，整字符丢弃
        assert!(out.text.is_char_boundary(out.text.len()));
        assert_eq!(out.total_bytes, 9);
    }

    #[tokio::test]
    async fn run_bounded_keeps_streams_separate_and_under_limit() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            // \346\227\245 = 日（八进制转义，POSIX printf 全平台可用）。
            .arg("printf 'out-\\346\\227\\245\\n'; printf 'err\\n' >&2; exit 7")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let run = run_bounded(child, 1024, Duration::from_secs(10), None)
            .await
            .expect("both pipes configured");
        assert_eq!(run.outcome, Outcome::Exited(Some(7)));
        assert_eq!(run.stdout.text, "out-日\n");
        assert!(!run.stdout.truncated);
        assert_eq!(run.stderr.text, "err\n");
        assert!(!run.stderr.truncated);
        assert!(!run.drained_short);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_overflow_kills_process_group_and_keeps_summary() {
        // 可控 fake child：`yes` 洪泛 stdout 永不自行退出，只有越限杀组才能收敛。
        let mut command = Command::new("yes");
        command
            .arg("abcdefghij")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn yes");
        let pgid = child.id().expect("child pid") as i32;

        let run = run_bounded(child, 1024, Duration::from_secs(30), None)
            .await
            .expect("pipes configured");

        assert_eq!(run.outcome, Outcome::Exited(None));
        assert!(run.stdout.truncated, "overflow state must be reported");
        assert!(run.stdout.total_bytes > 1024);
        assert!(run.stdout.text.len() <= 1024);
        assert!(run.stdout.text.starts_with("abcdefghij\n"));
        let note = run.stdout.truncation_note().expect("summary kept");
        assert!(note.contains("process group killed"), "{note}");
        assert!(!run.stderr.truncated);

        eventually("yes process group gone", || !signalable(-pgid)).await;
        eventually("yes process gone", || !signalable(pgid)).await;
    }

    #[tokio::test]
    async fn run_bounded_returns_error_for_unpiped_stream() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped()); // 故意漏配 stderr
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let err = run_bounded(child, 1024, Duration::from_secs(10), None)
            .await
            .expect_err("unpiped stderr must surface as error, not panic");
        let message = err.to_string();
        assert!(message.contains("stderr piped"), "{message}");
        assert!(message.contains("Stdio::piped()"), "{message}");
    }

    #[tokio::test]
    async fn wait_and_drain_reports_unpiped_child_as_error_not_panic() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped()); // 故意漏配 stdout
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn sh");
        let out = wait_and_drain(child, Duration::from_secs(10), None).await;
        assert_eq!(out.outcome, Outcome::Exited(None));
        assert!(
            out.stderr.contains("subprocess configuration error"),
            "{}",
            out.stderr
        );
        assert!(out.stderr.contains("stdout piped"), "{}", out.stderr);
    }

    #[tokio::test]
    async fn wait_and_drain_inlines_truncation_summary_when_over_default_cap() {
        let mut command = Command::new("yes");
        command
            .arg("0123456789")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = ProcessGroupChild::spawn(&mut command).expect("spawn yes");
        let out = wait_and_drain(child, Duration::from_secs(30), None).await;
        assert!(
            out.stdout.contains("output truncated: kept first"),
            "expected inline summary, got:\n{}",
            out.stdout
        );
        assert!(out.stdout.starts_with("0123456789\n"));
        assert!(out.stdout.len() <= DEFAULT_OUTPUT_CAP_BYTES + 256);
    }
}
