//! repo-improvements 09：CLI 二进制级集成测试；12（T5 / A6 / A7）：
//! stdout/stderr 契约、退出码、resume 跨命令、/clear /compact、Ctrl-C 与
//! 子进程组回收的回归。
//!
//! 经 `env!("CARGO_BIN_EXE_instagent")` 起真进程，每测试一套
//! `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` / `INSTAGENT_AGENTS_DIR`
//! 沙箱三变量隔离（tempdir，不改本测试进程 env，不触碰真实目录），
//! provider 用测试进程内的 wiremock SSE 顶替（同 `src/cli/assembly.rs`
//! 进程内测试的做法）。覆盖 README 手工验证清单第 4/5/7/8 条的可自动化部分。
//!
//! 交互路径（chat REPL）不依赖 PTY crate（依赖清单由 `00` 锁定）：
//! rustyline 对非 tty stdin 按行读取，管道喂入即可驱动斜杠命令与
//! Ctrl-C（`tokio::signal::ctrl_c` 与终端无关，`kill -INT` 直达进程）。
//! 全部离线跑通，不依赖 live API key；live 测试保持可选（`live_e2e.rs`）。
//!
//! 输出契约（ADR 0003 D4）：stdout 只放模型回答文本流；工具事件、预览、
//! usage、session id、notes、错误与一切诊断走 stderr；运行失败非零退出且
//! stderr 末行 `error: …`；stdout EPIPE 不改退出码。

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::Mutex;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const BIN: &str = env!("CARGO_BIN_EXE_instagent");
/// wiremock 秒回，但 CI 机器可能很慢；超时兜底防挂死。
const TIMEOUT: Duration = Duration::from_secs(120);
/// 交互测试里等待单条 stderr 标记的超时。
const WAIT: Duration = Duration::from_secs(60);

const PLUGIN_SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// 最简一轮：一条文本 delta + usage + stop + [DONE]。
const SIMPLE_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

fn sse_body() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(SIMPLE_SSE)
}

/// 假 openai provider 一次最简回复（POST /v1/chat/completions）。
async fn mount_chat_completions(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_body())
        .mount(server)
        .await;
}

/// 一轮 assistant 回复 = 一次 `shell` 工具调用（参数里带命令），无文本。
/// 用于 Ctrl-C 轮内取消测试：工具跑得够久才有取消窗口。
fn shell_call_sse(command: &str) -> ResponseTemplate {
    let args = serde_json::json!({ "command": command }).to_string();
    let call = serde_json::json!({ "choices": [{ "delta": {
        "role": "assistant",
        "tool_calls": [{
            "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "shell", "arguments": "" }
        }]
    }, "finish_reason": null }] });
    let args_frame = serde_json::json!({ "choices": [{ "delta": {
        "tool_calls": [{ "index": 0, "function": { "arguments": args } }]
    }, "finish_reason": null }] });
    let end = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
    let body = format!("data: {call}\n\ndata: {args_frame}\n\ndata: {end}\n\ndata: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// 本地插件夹具（含 hooks + command tool 两个会执行命令的面）。
fn trusty_plugin_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/trusty-plugin")
}

/// 每测试一个沙箱：三变量目录 + 独立 cwd，全部 tempdir。
struct Sandbox {
    config: TempDir,
    data: TempDir,
    agents: TempDir,
    cwd: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            config: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
            agents: TempDir::new().unwrap(),
            cwd: TempDir::new().unwrap(),
        }
    }

    /// 起真二进制的命令：cwd 与各目录变量都指向沙箱，
    /// 清掉外部可能泄漏进来的覆盖变量。
    fn cmd(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(BIN);
        cmd.args(args)
            .current_dir(self.cwd.path())
            .stdin(Stdio::null())
            .env("INSTAGENT_CONFIG_DIR", self.config.path())
            .env("INSTAGENT_DATA_DIR", self.data.path())
            .env("INSTAGENT_AGENTS_DIR", self.agents.path())
            .env_remove("INSTAGENT_PROVIDER")
            .env_remove("INSTAGENT_MODEL")
            .env_remove("RUST_LOG");
        cmd
    }

    fn write_config_yaml(&self, yaml: &str) {
        std::fs::write(self.config.path().join("config.yaml"), yaml).unwrap();
    }

    /// 用户插件 `fakeprov`：openai 引擎 provider 指向 wiremock，
    /// config.yaml 选它 + 给定模型。
    fn install_fake_provider(&self, base_url: &str) {
        let dir = self.agents.path().join("plugins").join("fakeprov");
        let providers = dir.join("dev.instagent").join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"fakeprov","version":"1.0.0"}}"#),
        )
        .unwrap();
        std::fs::write(
            providers.join("fake.json"),
            format!(r#"{{"name":"fake","engine":"openai","base_url":"{base_url}/v1"}}"#),
        )
        .unwrap();
        self.write_config_yaml("provider: fake\nmodel: test-model\n");
    }

    fn sessions_dir(&self) -> PathBuf {
        self.data.path().join("sessions")
    }
}

/// 跑完拿输出：进程组 + `kill_on_drop(true)`（仓库约定），整体超时兜底。
async fn output(mut cmd: tokio::process::Command) -> Output {
    instagent::subprocess::configure_subprocess(&mut cmd);
    tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .expect("instagent 二进制执行超时")
        .expect("spawn instagent 二进制")
}

fn assert_ok(output: &Output, ctx: &str) {
    assert!(
        output.status.success(),
        "{ctx}: 期望退出码 0，实得 {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// 失败退出断言（D4）：非零退出码，且 stderr 最后一个非空行以 `error:` 开头。
fn assert_failed_with_error_line(output: &Output, ctx: &str) {
    assert!(
        !output.status.success(),
        "{ctx}: 期望非零退出码\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    assert!(
        last.starts_with("error:"),
        "{ctx}: stderr 末行应为 `error: …`，实为 `{last}`\n全部 stderr: {stderr}"
    );
}

// ---- 交互驱动（管道 stdin；rustyline 非 tty 按行读，见模块注释） ----

/// 运行中的 REPL 子进程：stdin 可写（`None` = 已关闭送 EOF），
/// stdout 后台抽干，stderr 按行断言。
struct Repl {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stderr: BufReader<tokio::process::ChildStderr>,
    stderr_seen: Vec<String>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stdout_task: tokio::task::JoinHandle<()>,
}

impl Sandbox {
    /// 起一个可交互的 chat 子进程（三管道都接管）。
    async fn spawn_repl(&self, args: &[&str]) -> Repl {
        let mut cmd = self.cmd(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        instagent::subprocess::configure_subprocess(&mut cmd);
        let mut child = cmd.spawn().expect("spawn instagent chat");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let drained = stdout_buf.clone();
        let stdout_task = tokio::spawn(async move {
            let mut reader = stdout;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => drained.lock().await.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        });
        Repl {
            child,
            stdin: Some(stdin),
            stderr,
            stderr_seen: Vec::new(),
            stdout: stdout_buf,
            stdout_task,
        }
    }
}

impl Repl {
    /// 等 stderr 出现含 `needle` 的行（已读过的行也会复查）。
    async fn wait_for(&mut self, needle: &str) {
        let poll = async {
            loop {
                if self.stderr_seen.iter().any(|line| line.contains(needle)) {
                    return;
                }
                let mut line = String::new();
                let n = self.stderr.read_line(&mut line).await.expect("read stderr");
                assert!(
                    n > 0,
                    "stderr EOF，未等到 `{needle}`；已见 {:?}",
                    self.stderr_seen
                );
                self.stderr_seen.push(line.trim_end().to_string());
            }
        };
        tokio::time::timeout(WAIT, poll).await.unwrap_or_else(|_| {
            panic!(
                "等待 `{needle}` 超时（{}s）；已见 stderr: {:?}",
                WAIT.as_secs(),
                self.stderr_seen
            )
        });
    }

    async fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(line.as_bytes()).await.expect("write stdin");
        stdin.flush().await.expect("flush stdin");
    }

    /// 关闭 stdin（送 EOF），触发 Ctrl-D 退出路径。
    async fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// 等进程退出（要求成功），收齐 stdout 与全部已见 stderr。
    async fn finish_ok(mut self, ctx: &str) -> (String, Vec<String>) {
        let status = tokio::time::timeout(TIMEOUT, self.child.wait())
            .await
            .unwrap_or_else(|_| panic!("{ctx}: 等待退出超时；已见 stderr: {:?}", self.stderr_seen))
            .expect("reap child");
        assert!(
            status.success(),
            "{ctx}: 期望退出码 0，实得 {:?}；已见 stderr: {:?}",
            status.code(),
            self.stderr_seen
        );
        // 子进程已退出 → stdout 已 EOF，抽干任务随即结束。
        let _ = tokio::time::timeout(Duration::from_secs(10), self.stdout_task).await;
        let stdout = String::from_utf8(self.stdout.lock().await.clone()).expect("utf8 stdout");
        (stdout, self.stderr_seen)
    }

    #[cfg(unix)]
    fn pid(&self) -> u32 {
        self.child.id().expect("child pid")
    }
}

#[cfg(unix)]
async fn sigint(pid: u32) {
    let status = tokio::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("run kill -INT");
    assert!(status.success(), "kill -INT {pid}");
}

/// pid 文件出现（孙进程已启动）后返回其 pid。
#[cfg(unix)]
async fn wait_pid_file(path: &Path) -> u32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                if pid > 0 {
                    return pid;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid 文件始终未出现: {path:?}");
}

/// `kill -0` 轮询直到进程不存在（进程组回收断言，同 shell 单测做法）。
#[cfg(unix)]
async fn eventually_dead(pid: u32) {
    for _ in 0..100 {
        let status = tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .expect("run kill -0");
        if !status.success() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid {pid} 在取消后仍存活（子进程组未回收）");
}

// ---- T2：run -t 全链路 + stdout/stderr 契约（D4） ----

#[tokio::test]
async fn run_task_full_chain_prints_reply_and_usage() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // stdout 只有答案文本流（含收尾换行）：管道消费方拿到纯答案。
    assert_eq!(stdout, "hi there\n", "stdout 必须是纯答案");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.lines().any(|line| line.starts_with("session ")),
        "{stderr}"
    );
    assert!(stderr.contains("usage: in=12 out=5"), "{stderr}");
}

/// 工具轮：工具事件 / 预览 / usage 全部 stderr，stdout 仍只有答案文本。
#[tokio::test]
async fn run_tool_turn_keeps_stdout_pure() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    // 第一次请求 → shell 工具调用；第二次（带工具结果）→ 文本回复。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse("echo contract"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let out = output(sandbox.cmd(&["run", "-t", "run echo"])).await;
    assert_ok(&out, "run -t (tool turn)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "hi there\n", "工具事件不得进 stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("▶ shell"), "{stderr}");
    assert!(stderr.contains("echo contract"), "{stderr}");
    assert!(
        stderr.lines().any(|line| line.starts_with("usage: ")),
        "{stderr}"
    );
}

/// stdout 管道提前关闭（EPIPE）：写失败被忽略，退出码不变（D4）。
#[tokio::test]
async fn run_stdout_epipe_does_not_change_exit_code() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let mut cmd = sandbox.cmd(&["run", "-t", "say hi"]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    instagent::subprocess::configure_subprocess(&mut cmd);
    let mut child = cmd.spawn().expect("spawn run");
    drop(child.stdout.take()); // 立刻关闭读端 → 答案流写入撞 EPIPE。
    let status = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .expect("wait run")
        .expect("reap run");
    assert!(
        status.success(),
        "EPIPE 不应改变退出码，实得 {:?}",
        status.code()
    );
}

// ---- 坏参数 / 坏状态：退出码与错误行 ----

#[tokio::test]
async fn bad_arguments_exit_nonzero_with_error() {
    let sandbox = Sandbox::new();

    // 缺必填 -t。
    let out = output(sandbox.cmd(&["run"])).await;
    assert!(!out.status.success(), "缺 -t 应非零退出");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 未知顶层参数。
    let out = output(sandbox.cmd(&["--no-such-flag"])).await;
    assert!(!out.status.success(), "未知参数应非零退出");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // sessions rm 缺 id。
    let out = output(sandbox.cmd(&["sessions", "rm"])).await;
    assert!(!out.status.success(), "rm 缺 id 应非零退出");
}

#[tokio::test]
async fn run_without_provider_fails_with_error_last_line() {
    let sandbox = Sandbox::new();
    sandbox.write_config_yaml(""); // 无 provider/model。
    let out = output(sandbox.cmd(&["run", "-t", "hi"])).await;
    assert_failed_with_error_line(&out, "无 provider 配置");
    assert!(out.stdout.is_empty(), "失败时 stdout 必须为空");
}

/// provider 连接失败（连不上）：运行失败 → 非零退出 + 末行 `error:`（D4）。
#[tokio::test]
async fn provider_connection_failure_exits_nonzero() {
    let sandbox = Sandbox::new();
    // 9 = discard 端口，连接必被拒。
    sandbox.install_fake_provider("http://127.0.0.1:9");
    let out = output(sandbox.cmd(&["run", "-t", "hi"])).await;
    assert_failed_with_error_line(&out, "provider 连接失败");
    assert!(out.stdout.is_empty(), "失败时 stdout 必须为空");
}

/// MCP server 起不来：可见 note（stderr），不 fatal（todo 10 语义）。
#[tokio::test]
async fn mcp_failure_is_visible_note_but_not_fatal() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    // 插件 `mcpbroken`：provider 已在 `fakeprov`，这里只放起不来的 MCP。
    let dir = sandbox.agents.path().join("plugins").join("mcpbroken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"mcpbroken","version":"1.0.0"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("mcp.json"),
        r#"{"mcpServers":{"broken":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}}}"#,
    )
    .unwrap();

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（MCP 失败不应挡启动）");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi there\n",
        "MCP 失败不影响纯答案契约"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("note: MCP servers of plugin `mcpbroken`"),
        "{stderr}"
    );
    assert!(stderr.contains("failed to start"), "{stderr}");
}

// ---- sessions list / rm（原 T3） ----

#[tokio::test]
async fn sessions_list_and_rm_round_trip() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    // 先经 run -t 产生一个会话（写入沙箱 <data>/sessions/）。
    let run = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&run, "run -t");
    let stderr = String::from_utf8_lossy(&run.stderr);
    let id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .expect("run 应在 stderr 打出 session id")
        .trim()
        .to_string();
    let file = sandbox.sessions_dir().join(format!("{id}.jsonl"));
    assert!(file.is_file(), "会话文件应落在沙箱数据目录: {file:?}");

    let list = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&list, "sessions list");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains(&id), "{stdout}");
    assert!(stdout.contains("fake/test-model"), "{stdout}");

    let rm = output(sandbox.cmd(&["sessions", "rm", &id])).await;
    assert_ok(&rm, "sessions rm");
    let stdout = String::from_utf8_lossy(&rm.stdout);
    assert!(
        stdout.contains(&format!("removed session {id}")),
        "{stdout}"
    );
    assert!(!file.exists(), "rm 后会话文件应被删除");

    let list = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&list, "sessions list (空)");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("(no sessions)"),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );

    // 再删同一 id：报错、非零退出码。
    let rm = output(sandbox.cmd(&["sessions", "rm", &id])).await;
    assert!(!rm.status.success(), "rm 不存在的会话应以错误退出");
}

// ---- plugin install / list / show / disable / enable（原 T4） ----

#[tokio::test]
async fn plugin_install_list_show_disable_enable() {
    let sandbox = Sandbox::new();
    let source = trusty_plugin_fixture().display().to_string();

    let install = output(sandbox.cmd(&["plugin", "install", &source])).await;
    assert_ok(&install, "plugin install");
    let stdout = String::from_utf8_lossy(&install.stdout);
    assert!(stdout.contains("installed `trustme` v1.0.0"), "{stdout}");

    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    assert_ok(&list, "plugin list");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("trustme"), "{stdout}");
    assert!(stdout.contains("enabled"), "{stdout}");

    let show = output(sandbox.cmd(&["plugin", "show", "trustme"])).await;
    assert_ok(&show, "plugin show");
    let stdout = String::from_utf8_lossy(&show.stdout);
    for expected in [
        "name: trustme",
        "version: 1.0.0",
        "enabled: true",
        "source: ",
        "auto-update: false",
    ] {
        assert!(stdout.contains(expected), "show 缺 `{expected}`: {stdout}");
    }

    let disable = output(sandbox.cmd(&["plugin", "disable", "trustme"])).await;
    assert_ok(&disable, "plugin disable");
    assert!(String::from_utf8_lossy(&disable.stdout).contains("disabled trustme"));
    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("disabled"), "{stdout}");

    let enable = output(sandbox.cmd(&["plugin", "enable", "trustme"])).await;
    assert_ok(&enable, "plugin enable");
    assert!(String::from_utf8_lossy(&enable.stdout).contains("enabled trustme"));
    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("enabled"), "{stdout}");
}

// ---- --plugin PATH 临时加载（原 T5） ----

#[tokio::test]
async fn run_with_plugin_flag_loads_dev_provider() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;

    // 开发插件只经 --plugin 传入，不进用户安装目录。
    let dev = sandbox.cwd.path().join("devplugin");
    let providers = dev.join("dev.instagent").join("providers");
    std::fs::create_dir_all(&providers).unwrap();
    std::fs::write(
        dev.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"devfake","version":"0.1.0"}}"#),
    )
    .unwrap();
    std::fs::write(
        providers.join("fake.json"),
        format!(
            r#"{{"name":"fake","engine":"openai","base_url":"{}/v1"}}"#,
            server.uri()
        ),
    )
    .unwrap();
    sandbox.write_config_yaml("provider: fake\nmodel: test-model\n");

    let out = output(sandbox.cmd(&["run", "-t", "say hi", "--plugin", "devplugin"])).await;
    assert_ok(&out, "run --plugin devplugin");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "hi there\n", "{stdout}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage: in=12 out=5"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- resume 跨命令（A7） ----

#[tokio::test]
async fn resume_across_commands_continues_same_session() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    // run -t 建会话。
    let run = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&run, "run -t");
    let id = String::from_utf8_lossy(&run.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .expect("session id on stderr")
        .trim()
        .to_string();

    // chat --resume <id>：横幅确认同一会话，再跑一轮。
    let mut repl = sandbox.spawn_repl(&["chat", "--resume", &id]).await;
    repl.wait_for(&format!("session {id}")).await;
    repl.send("again\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/exit\n").await;
    let (stdout, _) = repl.finish_ok("chat --resume").await;
    assert_eq!(stdout, "hi there\n", "resume 轮的答案同样只走 stdout");

    // 会话累计两条问答（header + 4 条消息），list 仍只有一条会话。
    let file = sandbox.sessions_dir().join(format!("{id}.jsonl"));
    let lines = std::fs::read_to_string(&file).unwrap();
    assert_eq!(lines.lines().count(), 5, "header + 4 messages: {lines}");
    let list = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&list, "sessions list");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "resume 不应新建会话: {stdout}"
    );

    // --resume last 选中同一会话（EOF 直接退出，无新轮次）。
    let mut repl = sandbox.spawn_repl(&["chat", "--resume", "last"]).await;
    repl.wait_for(&format!("session {id}")).await;
    repl.close_stdin().await;
    let _ = repl.finish_ok("chat --resume last (EOF)").await;
}

#[tokio::test]
async fn resume_bad_id_fails_with_error_last_line() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_provider("http://127.0.0.1:9"); // 无需真连上。
    let out = output(sandbox.cmd(&["chat", "--resume", "no-such-session"])).await;
    assert_failed_with_error_line(&out, "resume 不存在的会话");

    // 路径穿越 id 在边界被拒（02 的白名单），同样非零 + 末行 error:。
    let out = output(sandbox.cmd(&["chat", "--resume", "../evil"])).await;
    assert_failed_with_error_line(&out, "resume 路径穿越 id");
}

// ---- chat 斜杠命令：/clear /compact（A7，离线） ----

#[tokio::test]
async fn chat_clear_and_compact_slash_commands() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await; // 轮次 + 压缩摘要共用同一回复形状。
    sandbox.install_fake_provider(&server.uri());

    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("say hi\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/compact\n").await;
    repl.wait_for("· compacted").await;
    repl.wait_for("(compacted)").await;
    repl.send("/clear\n").await;
    repl.wait_for("(context cleared)").await;
    repl.send("/exit\n").await;
    let (stdout, stderr) = repl.finish_ok("chat /clear /compact").await;

    assert_eq!(stdout, "hi there\n", "斜杠命令反馈不得进 stdout");
    assert!(
        !stderr.iter().any(|line| line.contains("hi there")),
        "答案文本不得回流 stderr: {stderr:?}"
    );
    // /compact 与 /clear 的 rewrite 都留时间戳备份（02 语义）；主会话文件
    // 只有一个，且 /clear 后只剩 header 行。
    let dir = sandbox.sessions_dir();
    let main_files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.to_string_lossy().contains(".bak"))
        .collect();
    assert_eq!(main_files.len(), 1, "{main_files:?}");
    let content = std::fs::read_to_string(&main_files[0]).unwrap();
    assert_eq!(
        content.lines().count(),
        1,
        "/clear 后只留 header: {content}"
    );
}

#[tokio::test]
async fn chat_unknown_command_feedback_on_stderr() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_provider("http://127.0.0.1:9"); // 不跑轮次，无需连上。
    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("/nope\n").await;
    repl.wait_for("unknown command /nope").await;
    repl.send("/exit\n").await;
    let (stdout, _) = repl.finish_ok("chat unknown command").await;
    assert!(stdout.is_empty(), "stdout 无答案时应为空: {stdout:?}");
}

// ---- Ctrl-C（A7 / T5；unix） ----
//
// 空闲提示符的 Ctrl-C（rustyline `Interrupted`）只在真终端/PTY 下产生，
// 管道驱动无法触发；本仓库依赖清单由 00 锁定、不引 PTY crate，故该分支
// 不在 CI 覆盖。轮内取消（`watch_ctrl_c` 经 `tokio::signal::ctrl_c`）与
// 终端无关，`kill -INT` 即可驱动，见下测。

/// 轮内 Ctrl-C：取消当前轮（REPL 可继续），运行中的 shell 进程组被整组
/// 回收——孙进程（`sleep 120`）在退出后可观测地消失。
#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_midturn_cancels_and_reaps_process_group() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    // 唯一一次请求 = shell 工具调用；取消后不应有第二次请求。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse(
            "sleep 120 & echo $! > grandchild.pid; sleep 120",
        ))
        .mount(&server)
        .await;
    sandbox.install_fake_provider(&server.uri());

    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("go\n").await;
    repl.wait_for("▶ shell").await;

    let pid_file = sandbox.cwd.path().join("grandchild.pid");
    let grandchild = wait_pid_file(&pid_file).await;
    sigint(repl.pid()).await;
    repl.wait_for("(turn cancelled; Ctrl-C again to quit)")
        .await;

    repl.send("/exit\n").await;
    let (stdout, _) = repl.finish_ok("ctrl-c midturn").await;
    assert!(stdout.is_empty(), "被取消的轮没有答案文本: {stdout:?}");

    eventually_dead(grandchild).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "取消后不应再请求模型");
}

// ---- 08 T1：默认失败诊断可见（D01/D02；不设 RUST_LOG 即 warning 到 stderr） ----

/// 写一个只有 hook 的用户插件：`plugins/<name>/plugin.json` +
/// `dev.instagent/hooks.json`（单个事件单个 failing 命令）。
fn write_hook_plugin(agents: &Path, name: &str, event: &str, script_body: &str) {
    let dir = agents.join("plugins").join(name);
    let ns = dir.join("dev.instagent");
    std::fs::create_dir_all(&ns).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"}}"#),
    )
    .unwrap();
    std::fs::write(
        ns.join("hooks.json"),
        format!(
            r#"{{"hooks":{{"{event}":[{{"hooks":[{{"command":"${{PLUGIN_ROOT}}/fail.sh"}}]}}]}}}}"#
        ),
    )
    .unwrap();
    let script = dir.join("fail.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// 最小本地插件源（供 `plugin install <path>`，离线）。
fn write_minimal_plugin_source(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"}}"#),
    )
    .unwrap();
}

/// 失败 SessionStart hook：默认（无 RUST_LOG）stderr 有 warning 且成功继续；
/// 显式 `RUST_LOG=off` 则保持安静（过滤设置优先）。
#[tokio::test]
async fn hook_session_start_failure_warns_by_default_but_respects_rust_log() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    write_hook_plugin(
        sandbox.agents.path(),
        "hookfail",
        "SessionStart",
        "mkdir -p \"${PLUGIN_ROOT}/.hook-out\"; touch \"${PLUGIN_ROOT}/.hook-out/started\"; exit 1",
    );

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（SessionStart hook 失败不挡启动）");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi there\n",
        "hook 诊断不得进 stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SessionStart"),
        "默认应有 SessionStart 诊断: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("warn"),
        "失败 hook 应以 warning 可见: {stderr}"
    );
    assert!(
        sandbox
            .agents
            .path()
            .join("plugins")
            .join("hookfail")
            .join(".hook-out")
            .join("started")
            .is_file(),
        "hook 脚本应已执行（非 spawn 失败）"
    );

    // 显式过滤优先：RUST_LOG=off 时同一失败保持安静，但仍成功。
    let mut cmd = sandbox.cmd(&["run", "-t", "say hi"]);
    cmd.env("RUST_LOG", "off");
    let out = output(cmd).await;
    assert_ok(&out, "run -t（RUST_LOG=off）");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("SessionStart"),
        "显式 off 应压住 hook 诊断: {stderr}"
    );
}

/// 失败 PreToolUse hook：fail-open（工具仍执行、退出 0），且默认有诊断。
#[tokio::test]
async fn hook_pre_tool_use_failure_is_fail_open_and_visible() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse("touch pretool-marker"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    write_hook_plugin(sandbox.agents.path(), "hookfail", "PreToolUse", "exit 1");

    let out = output(sandbox.cmd(&["run", "-t", "run it"])).await;
    assert_ok(&out, "run -t（PreToolUse hook 失败应 fail-open）");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi there\n",
        "答案仍只走 stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("PreToolUse"),
        "默认应有 PreToolUse 诊断: {stderr}"
    );
    assert!(
        sandbox.cwd.path().join("pretool-marker").is_file(),
        "fail-open：工具必须照常执行"
    );
}

/// 超大 SKILL.md：有界带路径 warning，健康路径照常跑通、stdout 干净。
#[tokio::test]
async fn oversized_skill_md_warns_but_healthy_path_still_works() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let skills = sandbox.agents.path().join("skills");
    let big = skills.join("bigskill");
    std::fs::create_dir_all(&big).unwrap();
    std::fs::write(big.join("SKILL.md"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let good = skills.join("goodskill");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(
        good.join("SKILL.md"),
        "---\nname: goodskill\ndescription: a healthy skill\n---\n\nDo things.\n",
    )
    .unwrap();

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（超大 skill 只跳过该来源）");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi there\n",
        "诊断不得进 stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bigskill"),
        "诊断应带超大 skill 路径: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("skill"),
        "诊断应指明 skill 来源: {stderr}"
    );
}

/// 不可读 SKILL.md：带路径 warning，健康路径照常跑通（unix；root 下跳过）。
#[cfg(unix)]
#[tokio::test]
async fn unreadable_skill_md_warns_but_healthy_path_still_works() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let skills = sandbox.agents.path().join("skills");
    let locked = skills.join("lockedskill");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(
        locked.join("SKILL.md"),
        "---\nname: lockedskill\ndescription: unreadable\n---\n\nBody.\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            locked.join("SKILL.md"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    if std::fs::read(locked.join("SKILL.md")).is_ok() {
        eprintln!("SKIP unreadable_skill_md：当前用户仍可读 000 文件（可能 root）");
        return;
    }

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（不可读 skill 只跳过该来源）");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi there\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("lockedskill"),
        "诊断应带不可读 skill 路径: {stderr}"
    );
}

/// MCP 失败 note 不带环境密钥回显。
#[tokio::test]
async fn mcp_failure_note_does_not_echo_env_secrets() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let dir = sandbox.agents.path().join("plugins").join("mcpbroken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"mcpbroken","version":"1.0.0"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("mcp.json"),
        r#"{"mcpServers":{"broken":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}}}"#,
    )
    .unwrap();

    let mut cmd = sandbox.cmd(&["run", "-t", "say hi"]);
    cmd.env("INSTAGENT_08_FAKE_MARKER", "RegTest08NoLeak-qwerty-98765");
    let out = output(cmd).await;
    assert_ok(&out, "run -t（MCP 失败不应挡启动）");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("note: MCP servers of plugin `mcpbroken`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("RegTest08NoLeak-qwerty-98765"),
        "MCP 诊断不得回显环境值: {stderr}"
    );
}

// ---- 08 T2：完整配置校验与手动压缩取消 ----

/// `-m '   '` 在 provider/MCP 启动前失败：零请求、无新会话、文件未改。
#[tokio::test]
async fn blank_model_override_fails_before_provider_start() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let config_path = sandbox.config.path().join("config.yaml");
    let before = std::fs::read(&config_path).unwrap();

    let out = output(sandbox.cmd(&["run", "-t", "hi", "-m", "   "])).await;
    assert_failed_with_error_line(&out, "空白 -m 应早期失败");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("model"), "{stderr}");
    assert!(out.stdout.is_empty(), "失败时 stdout 必须为空");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "校验失败不得请求模型"
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "配置文件不得被改"
    );
    let sessions = sandbox.sessions_dir();
    assert!(
        !sessions.exists() || std::fs::read_dir(&sessions).unwrap().next().is_none(),
        "校验失败不得新建会话"
    );
}

/// 下溢小阈值（f64 合法、f32 变 0）在启动前失败，文件未改。
#[tokio::test]
async fn underflow_compaction_threshold_fails_early() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let config_path = sandbox.config.path().join("config.yaml");
    std::fs::write(
        &config_path,
        "provider: fake\nmodel: test-model\ncompaction_threshold: 1e-46\n",
    )
    .unwrap();
    let before = std::fs::read(&config_path).unwrap();

    let out = output(sandbox.cmd(&["run", "-t", "hi"])).await;
    assert_failed_with_error_line(&out, "下溢阈值应早期失败");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("compaction_threshold"), "{stderr}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "校验失败不得请求模型"
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "配置文件不得被改"
    );
}

/// 超大 config：有界拒绝、带来源、不回显原文、文件未改、零请求。
#[tokio::test]
async fn oversized_config_fails_early_without_echo() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let config_path = sandbox.config.path().join("config.yaml");
    let filler = "QZ".repeat(600_000);
    let content = format!("provider: fake\nmodel: test-model\n# {filler}\n");
    assert!(content.len() as u64 > 1024 * 1024);
    std::fs::write(&config_path, &content).unwrap();

    let out = output(sandbox.cmd(&["run", "-t", "hi"])).await;
    assert_failed_with_error_line(&out, "超大 config 应早期失败");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config.yaml"), "{stderr}");
    assert!(stderr.contains("budget"), "{stderr}");
    assert!(
        !stderr.contains("QZQZQZQZ"),
        "config 错误不得回显原文: {}",
        &stderr[..stderr.len().min(400)]
    );
    assert_eq!(std::fs::read(&config_path).unwrap().len(), content.len());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "校验失败不得请求模型"
    );
}

/// /compact 慢摘要中 Ctrl-C：取消后无压缩痕迹，REPL 可继续（unix）。
#[cfg(unix)]
#[tokio::test]
async fn compact_cancel_mid_summary_keeps_session_usable() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    // 摘要请求（含压缩 prompt）延迟 30s，给取消留窗口；普通轮次秒回。
    let slow_summary = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_delay(Duration::from_secs(30))
        .set_body_string(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"compacted summary\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":7}}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Task Context"))
        .respond_with(slow_summary)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("hello\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/compact\n").await;
    // 等摘要请求到达 mock 再取消（确定性窗口，不靠 sleep 猜）。
    for _ in 0..200 {
        let count = server.received_requests().await.unwrap().len();
        if count >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "摘要请求应已发出"
    );
    sigint(repl.pid()).await;
    repl.wait_for("(compaction cancelled)").await;

    // 原会话可继续：再跑一轮成功后退出。
    repl.send("again\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/exit\n").await;
    let (stdout, _) = repl.finish_ok("compact cancel then continue").await;
    assert_eq!(
        stdout, "hi there\nhi there\n",
        "取消的压缩无答案，两轮答案完整"
    );

    // 无压缩痕迹：header + 4 条消息，无摘要行。
    let dir = sandbox.sessions_dir();
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert_eq!(content.lines().count(), 5, "header + 4 messages: {content}");
    assert!(
        !content.contains(instagent::message::SUMMARY_PREFIX),
        "取消的压缩不得留下摘要: {content}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "两轮 + 一次被取消的摘要");
}

// ---- 08 T3：将隔离复现变成 CLI 回归 ----

/// 裸 TCP 可控分块服务器：读完一个 HTTP 请求后把响应按给定两段写入
/// （中间 flush + 停顿），返回 base_url 与收到的请求计数。
async fn split_response_server(first: Vec<u8>, second: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        counter.fetch_add(1, Ordering::SeqCst);
        // 读请求头拿 Content-Length，再读完 body（内容忽略）。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut header_end = None;
        let mut content_len = 0usize;
        loop {
            let Ok(n) = sock.read(&mut tmp).await else {
                return;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]);
                for line in head.lines().skip(1) {
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        content_len = value.trim().parse().unwrap_or(0);
                    }
                }
                header_end = Some(pos + 4);
                break;
            }
        }
        let Some(end) = header_end else { return };
        while buf.len() < end + content_len {
            let Ok(n) = sock.read(&mut tmp).await else {
                return;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let _ = sock.write_all(&first).await;
        let _ = sock.flush().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = sock.write_all(&second).await;
        let _ = sock.flush().await;
    });
    (format!("http://{addr}"), hits)
}

/// P01 反转：SSE 把“中”拆在 UTF-8 第一个字节后，输出仍是完整“中文🙂”。
#[tokio::test]
async fn stream_split_utf8_boundary_preserved() {
    let sandbox = Sandbox::new();
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"中文🙂\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    // “中”（E4 B8 AD）第一个字节后切分（按字节切，不按字符边界）。
    let raw = response.as_bytes();
    let split = raw
        .windows("中".len())
        .position(|w| w == "中".as_bytes())
        .expect("body 含中文")
        + 1;
    let (first, second) = (raw[..split].to_vec(), raw[split..].to_vec());
    let (base_url, hits) = split_response_server(first, second).await;
    sandbox.install_fake_provider(&base_url);

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（UTF-8 切分）");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "中文🙂\n",
        "切分点不得产生替换符"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// 生成一轮 shell 工具调用的 SSE（无 `[DONE]` 由调用方决定是否追加）。
fn tool_call_sse(command: &str, id: &str, with_end_and_done: bool) -> String {
    let args = serde_json::json!({ "command": command }).to_string();
    let call = serde_json::json!({ "choices": [{ "delta": {
        "role": "assistant",
        "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": { "name": "shell", "arguments": "" }
        }]
    }, "finish_reason": null }] });
    let args_frame = serde_json::json!({ "choices": [{ "delta": {
        "tool_calls": [{ "index": 0, "function": { "arguments": args } }]
    }, "finish_reason": null }] });
    let mut body = format!("data: {call}\n\ndata: {args_frame}\n\n");
    if with_end_and_done {
        let end =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        body.push_str(&format!("data: {end}\n\ndata: [DONE]\n\n"));
    }
    body
}

/// P02 反转：工具流无完成标记（EOF 既无 finish_reason 也无 `[DONE]`），
/// 工具副作用前被拒绝：非零退出、标记文件不存在、只一次请求、输入保留。
#[tokio::test]
async fn tool_stream_without_completion_rejected_before_side_effect() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(tool_call_sse("touch p02-marker", "call_1", false)),
        )
        .mount(&server)
        .await;
    sandbox.install_fake_provider(&server.uri());

    let out = output(sandbox.cmd(&["run", "-t", "run it"])).await;
    assert_failed_with_error_line(&out, "无完成标记的工具流应失败");
    assert!(out.stdout.is_empty(), "失败时 stdout 必须为空");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("without completion") || stderr.contains("[DONE]"),
        "错误应指明流缺完成信号: {stderr}"
    );
    assert!(
        !sandbox.cwd.path().join("p02-marker").exists(),
        "无完成标记不得执行工具"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "拒绝后不应再请求模型"
    );
    let files: Vec<_> = std::fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert_eq!(content.lines().count(), 2, "输入保留可重试: {content}");
}

/// A01 反转：同一响应复用调用 ID，零副作用（两个标记都不存在）并错出。
#[tokio::test]
async fn duplicate_tool_call_ids_rejected_without_execution() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    let args_a = serde_json::json!({ "command": "touch dup-marker-a" }).to_string();
    let args_b = serde_json::json!({ "command": "touch dup-marker-b" }).to_string();
    let frames = [
        serde_json::json!({ "choices": [{ "delta": {
            "role": "assistant",
            "tool_calls": [
                { "index": 0, "id": "call_1", "type": "function",
                  "function": { "name": "shell", "arguments": "" } },
                { "index": 1, "id": "call_1", "type": "function",
                  "function": { "name": "shell", "arguments": "" } },
            ]
        }, "finish_reason": null }] }),
        serde_json::json!({ "choices": [{ "delta": {
            "tool_calls": [{ "index": 0, "function": { "arguments": args_a } }]
        }, "finish_reason": null }] }),
        serde_json::json!({ "choices": [{ "delta": {
            "tool_calls": [{ "index": 1, "function": { "arguments": args_b } }]
        }, "finish_reason": null }] }),
        serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
    ];
    let mut body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    body.push_str("data: [DONE]\n\n");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    sandbox.install_fake_provider(&server.uri());

    let out = output(sandbox.cmd(&["run", "-t", "run it"])).await;
    assert_failed_with_error_line(&out, "复用调用 ID 应失败");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reuses tool use id"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!sandbox.cwd.path().join("dup-marker-a").exists());
    assert!(!sandbox.cwd.path().join("dup-marker-b").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A02 反转：`hello` → `/compact` → `next` 全程成功，无 `consecutive User`。
#[tokio::test]
async fn compact_then_continue_keeps_session_valid() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await; // 轮次与压缩摘要共用同一回复形状。
    sandbox.install_fake_provider(&server.uri());

    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("hello\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/compact\n").await;
    repl.wait_for("(compacted)").await;
    repl.send("next\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/exit\n").await;
    let (stdout, stderr) = repl.finish_ok("compact then continue").await;
    assert_eq!(stdout, "hi there\nhi there\n", "两轮答案都在 stdout");
    assert!(
        !stderr.iter().any(|line| line.contains("consecutive")),
        "压缩后继续不得报 consecutive: {stderr:?}"
    );

    let files: Vec<_> = std::fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.to_string_lossy().contains(".bak"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert!(
        content.contains(instagent::message::SUMMARY_PREFIX),
        "压缩摘要应保留: {content}"
    );
    assert!(content.contains("next"), "新输入应并入摘要消息: {content}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "两轮 + 一次压缩摘要"
    );
}

/// A03 反转（取消后继续/恢复）：轮内 Ctrl-C 后 REPL 可继续，次轮成功（unix）。
#[cfg(unix)]
#[tokio::test]
async fn cancelled_turn_then_next_turn_succeeds() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse(
            "sleep 120 & echo $! > grandchild2.pid; sleep 120",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let mut repl = sandbox.spawn_repl(&["chat"]).await;
    repl.wait_for("instagent · provider fake").await;
    repl.send("go\n").await;
    repl.wait_for("▶ shell").await;

    let pid_file = sandbox.cwd.path().join("grandchild2.pid");
    let grandchild = wait_pid_file(&pid_file).await;
    sigint(repl.pid()).await;
    repl.wait_for("(turn cancelled; Ctrl-C again to quit)")
        .await;

    // 取消后继续：次轮成功。
    repl.send("again\n").await;
    repl.wait_for("usage: in=12 out=5").await;
    repl.send("/exit\n").await;
    let (stdout, _) = repl.finish_ok("cancel then continue").await;
    assert_eq!(stdout, "hi there\n", "被取消的轮无答案，次轮答案完整");

    eventually_dead(grandchild).await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "取消一轮 + 成功一轮"
    );
}

/// I01 反转 a：`enabledPlugins: []` 后 `plugin enable` 能写进白名单。
#[tokio::test]
async fn blank_whitelist_enable_writes_alpha() {
    let sandbox = Sandbox::new();
    let source = trusty_plugin_fixture().display().to_string();
    let install = output(sandbox.cmd(&["plugin", "install", &source])).await;
    assert_ok(&install, "plugin install");

    std::fs::write(
        sandbox.config.path().join("settings.json"),
        r#"{"enabledPlugins":[]}"#,
    )
    .unwrap();
    let enable = output(sandbox.cmd(&["plugin", "enable", "trustme"])).await;
    assert_ok(&enable, "plugin enable（空白名单）");
    let settings = std::fs::read_to_string(sandbox.config.path().join("settings.json")).unwrap();
    assert!(
        settings.contains("trustme"),
        "白名单应写入插件名: {settings}"
    );

    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    assert_ok(&list, "plugin list");
    let stdout = String::from_utf8_lossy(&list.stdout);
    let line = stdout
        .lines()
        .find(|line| line.contains("trustme"))
        .expect("list 应含 trustme: {stdout}");
    assert!(line.contains("enabled"), "{line}");
    assert!(!line.contains("disabled"), "{line}");
}

/// I01 反转 b：用户层白名单仅 alpha、项目层禁用 alpha；新装 beta 仍禁用。
#[tokio::test]
async fn cross_layer_clear_keeps_beta_disabled() {
    let sandbox = Sandbox::new();
    let alpha_src = sandbox.cwd.path().join("src-alpha");
    let beta_src = sandbox.cwd.path().join("src-beta");
    write_minimal_plugin_source(&alpha_src, "alpha");
    write_minimal_plugin_source(&beta_src, "beta");
    for src in [&alpha_src, &beta_src] {
        let out = output(sandbox.cmd(&["plugin", "install", &src.display().to_string()])).await;
        assert_ok(&out, "plugin install");
    }

    std::fs::write(
        sandbox.config.path().join("settings.json"),
        r#"{"enabledPlugins":["alpha"]}"#,
    )
    .unwrap();
    let project_settings = sandbox.cwd.path().join(".config").join("instagent");
    std::fs::create_dir_all(&project_settings).unwrap();
    std::fs::write(
        project_settings.join("settings.json"),
        r#"{"disabledPlugins":["alpha"]}"#,
    )
    .unwrap();

    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    assert_ok(&list, "plugin list（跨层清空）");
    let stdout = String::from_utf8_lossy(&list.stdout);
    for name in ["alpha", "beta"] {
        let line = stdout
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("list 应含 {name}: {stdout}"));
        assert!(
            line.contains("disabled"),
            "白名单被清空后 {name} 必须禁用（白名单不得退化成黑名单）: {line}"
        );
    }
}

/// S01 反转：损坏 session header 含假密钥形态，list 只报路径诊断、不回显。
#[tokio::test]
async fn corrupt_session_header_does_not_echo_fake_key() {
    let sandbox = Sandbox::new();
    let sessions = sandbox.sessions_dir();
    std::fs::create_dir_all(&sessions).unwrap();
    let marker = "sk-TESTSECRET08-abcdefgh";
    std::fs::write(
        sessions.join("deadbeef-1234.jsonl"),
        format!("{{\"id\": \"deadbeef-1234\", \"leak\": \"{marker}\"\n"),
    )
    .unwrap();

    let out = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&out, "sessions list（坏文件只跳过）");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("(no sessions)"), "{stdout}");
    assert!(!stdout.contains(marker), "stdout 不得回显假密钥");
    assert!(
        !stderr.contains(marker),
        "stderr 诊断不得回显损坏行: {stderr}"
    );
    assert!(stderr.contains("warning"), "应有带路径的跳过诊断: {stderr}");
}

/// I02 反转：安装异插件不删除无关恢复副本 `.replaced-lost-*`。
#[tokio::test]
async fn install_preserves_unrelated_recovery_backup() {
    let sandbox = Sandbox::new();
    let backup = sandbox
        .agents
        .path()
        .join("plugins")
        .join(".replaced-lost-00000000-1111-2222-3333-444444444444");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(
        backup.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"lost","version":"0.9.0"}}"#),
    )
    .unwrap();
    std::fs::write(backup.join("SENTINEL"), "recovery copy\n").unwrap();

    let beta_src = sandbox.cwd.path().join("src-beta");
    write_minimal_plugin_source(&beta_src, "beta");
    let out = output(sandbox.cmd(&["plugin", "install", &beta_src.display().to_string()])).await;
    assert_ok(&out, "plugin install beta");
    assert!(
        backup.join("SENTINEL").is_file(),
        "异插件恢复副本必须保留: {backup:?}"
    );

    let list = output(sandbox.cmd(&["plugin", "list"])).await;
    assert_ok(&list, "plugin list");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("beta"),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
}

/// I03 反转：`<data>/bundled/` 旧布局幽灵文件不进入运行时（保留在盘上）。
#[tokio::test]
async fn bundled_ignores_legacy_ghost_files() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    // 旧布局残留：直接放在缓存父目录下的 provider 定义。
    let ghost_dir = sandbox
        .data
        .path()
        .join("bundled")
        .join("dev.instagent")
        .join("providers");
    std::fs::create_dir_all(&ghost_dir).unwrap();
    std::fs::write(
        ghost_dir.join("ghost.json"),
        r#"{"name":"ghost","engine":"openai","base_url":"http://127.0.0.1:9/v1"}"#,
    )
    .unwrap();

    // 健康路径不受影响。
    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t（幽灵文件不得影响启动）");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi there\n");
    assert!(ghost_dir.join("ghost.json").is_file(), "旧文件保留在盘上");

    // 幽灵 provider 不可被选中：未知而非连不上。
    sandbox.write_config_yaml("provider: ghost\nmodel: test-model\n");
    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_failed_with_error_line(&out, "幽灵 provider 不应被加载");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown provider `ghost`"),
        "应报未知 provider: {stderr}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "仅健康那轮请求过模型"
    );
}
