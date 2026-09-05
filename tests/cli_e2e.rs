//! Headless CLI 二进制回归：完整任务输入、终态 JSON/退出码、插件能力、
//! 跨进程恢复、自动压缩、取消/超时与子进程组清理。
//!
//! 每测试使用独立目录与离线 wiremock provider；不依赖 stdin、TTY 或凭据。
//! stdout 仅输出答案或单个 JSON 文档，运行诊断统一走 stderr。

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const BIN: &str = env!("CARGO_BIN_EXE_instagent");
/// wiremock 秒回，但 CI 机器可能很慢；超时兜底防挂死。
const TIMEOUT: Duration = Duration::from_secs(120);

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

/// 启动一个独立批次并收集管道输出；调用者可以在结束前发送信号。
fn spawn_run(mut cmd: tokio::process::Command) -> tokio::process::Child {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    instagent::subprocess::configure_subprocess(&mut cmd);
    cmd.spawn().expect("spawn instagent run")
}

async fn finish_run(child: tokio::process::Child) -> Output {
    tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .expect("run did not terminate")
        .expect("reap run")
}

fn terminal_json(out: &Output, status: &str, exit_code: i32) -> serde_json::Value {
    assert_eq!(
        out.status.code(),
        Some(exit_code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // from_slice rejects trailing text or multiple documents.
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid terminal JSON: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    let object = value.as_object().expect("terminal object");
    assert_eq!(object.len(), 6, "{value}");
    for key in [
        "schema_version",
        "status",
        "session_id",
        "output",
        "usage",
        "error",
    ] {
        assert!(object.contains_key(key), "missing {key}: {value}");
    }
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], status);
    assert!(value["output"].is_string());
    if status == "completed" {
        assert!(value["error"].is_null(), "{value}");
    } else {
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty()),
            "{value}"
        );
    }
    value
}

fn session_messages(sandbox: &Sandbox, id: &str) -> Vec<instagent::message::Message> {
    let content =
        std::fs::read_to_string(sandbox.sessions_dir().join(format!("{id}.jsonl"))).unwrap();
    let messages: Vec<_> = content
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    instagent::message::validate(&messages).expect("persisted session remains valid");
    messages
}

async fn wait_requests(server: &MockServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if server.received_requests().await.unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("provider request did not arrive");
}

#[cfg(unix)]
async fn signal(pid: u32, signal: &str) {
    let mut cmd = tokio::process::Command::new("kill");
    cmd.args([signal, &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    instagent::subprocess::configure_subprocess(&mut cmd);
    assert!(
        cmd.status().await.expect("run kill").success(),
        "kill {signal} {pid}"
    );
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
        let mut cmd = tokio::process::Command::new("kill");
        cmd.args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        instagent::subprocess::configure_subprocess(&mut cmd);
        let status = cmd.status().await.expect("run kill -0");
        if !status.success() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Reap the known fixture child even when the assertion exposes a cleanup regression.
    signal(pid, "-KILL").await;
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

// ---- Headless 输入、终态结果与跨进程恢复 ----

#[tokio::test]
async fn json_completed_is_single_document_with_usage_and_session() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let out = output(sandbox.cmd(&["run", "-t", "say hi", "--output", "json"])).await;
    let result = terminal_json(&out, "completed", 0);
    assert_eq!(result["output"], "hi there");
    assert_eq!(
        result["usage"],
        serde_json::json!({
            "input":12,"output":5,"cache_read":0,"cache_write":0,
        })
    );
    let id = result["session_id"].as_str().expect("session id");
    assert_eq!(session_messages(&sandbox, id).len(), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("session {id}")));
}

#[tokio::test]
async fn json_setup_and_provider_failures_are_machine_readable() {
    let sandbox = Sandbox::new();
    sandbox.write_config_yaml("");
    let out = output(sandbox.cmd(&["run", "-t", "go", "--output", "json"])).await;
    let result = terminal_json(&out, "failed", 1);
    assert!(result["session_id"].is_null());
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());

    sandbox.install_fake_provider("http://127.0.0.1:9");
    let out = output(sandbox.cmd(&["run", "-t", "go", "--output", "json"])).await;
    let result = terminal_json(&out, "failed", 1);
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());
    let id = result["session_id"]
        .as_str()
        .expect("failed run session id");
    assert_eq!(session_messages(&sandbox, id).len(), 1);
}

#[tokio::test]
async fn json_max_turns_is_not_reported_as_completed() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse("echo bounded"))
        .mount(&server)
        .await;
    sandbox.install_fake_provider(&server.uri());
    sandbox.write_config_yaml("provider: fake\nmodel: test-model\nmax_turns: 1\n");
    let out = output(sandbox.cmd(&["run", "-t", "go", "--output", "json"])).await;
    let result = terminal_json(&out, "max_turns", 3);
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());
    let messages = session_messages(&sandbox, result["session_id"].as_str().unwrap());
    assert_eq!(messages.len(), 3, "task + tool call + tool result");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn task_source_and_timeout_arguments_are_validated_without_stdin() {
    let sandbox = Sandbox::new();
    for args in [
        vec!["run"],
        vec!["chat"],
        vec!["run", "-t", "hi", "--task-file", "task.md"],
        vec!["run", "-t", "hi", "--command", "plug:task"],
        vec!["run", "--task-file", "task.md", "--command", "plug:task"],
        vec!["run", "-t", "hi", "--args", "unused"],
        vec!["run", "-t", "hi", "--timeout", "0"],
        vec!["run", "-t", "hi", "--timeout", "-1"],
        vec!["run", "-t", "hi", "--timeout", "1.5"],
        vec!["run", "-t", "hi", "--output", "xml"],
        vec!["run", "--resume", "last"],
    ] {
        let out = output(sandbox.cmd(&args)).await;
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{args:?}: clap writes only stderr");
        assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
    }
}

#[tokio::test]
async fn blank_task_fails_with_json_before_provider_setup() {
    let sandbox = Sandbox::new();
    for task in ["", " \n\t"] {
        let out = output(sandbox.cmd(&["run", "-t", task, "--output", "json"])).await;
        let result = terminal_json(&out, "failed", 1);
        assert!(result["session_id"].is_null());
        assert_eq!(result["output"], "");
        assert!(
            result["error"].as_str().unwrap().contains("task"),
            "{result}"
        );
    }
}

#[tokio::test]
async fn task_file_loads_utf8_and_rejects_missing_blank_or_nonregular_input() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    std::fs::write(
        sandbox.cwd.path().join("task.md"),
        "TASK_FILE_蓝莓\nsecond line",
    )
    .unwrap();
    let out = output(sandbox.cmd(&["run", "--task-file", "task.md", "--output", "json"])).await;
    terminal_json(&out, "completed", 0);
    let request = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body["messages"].to_string().contains("TASK_FILE_蓝莓"));
    std::fs::write(sandbox.cwd.path().join("blank.md"), " \n\t").unwrap();
    std::fs::write(sandbox.cwd.path().join("binary.md"), [0xff, 0xfe]).unwrap();
    for name in ["missing.md", "blank.md", "binary.md", ".", "-"] {
        let out = output(sandbox.cmd(&["run", "--task-file", name, "--output", "json"])).await;
        let result = terminal_json(&out, "failed", 1);
        assert!(result["session_id"].is_null(), "{name}: {result}");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn task_file_fifo_is_rejected_without_opening_a_reader() {
    let sandbox = Sandbox::new();
    let fifo = sandbox.cwd.path().join("task.fifo");
    let mut cmd = tokio::process::Command::new("mkfifo");
    cmd.arg(&fifo).stdin(Stdio::null());
    instagent::subprocess::configure_subprocess(&mut cmd);
    assert!(cmd.status().await.unwrap().success());
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        output(sandbox.cmd(&["run", "--task-file", "task.fifo", "--output", "json"])),
    )
    .await
    .expect("FIFO task input must not wait for a writer");
    terminal_json(&out, "failed", 1);
}

#[tokio::test]
async fn run_finishes_even_when_stdin_pipe_remains_open() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let mut cmd = sandbox.cmd(&["run", "-t", "go", "--output", "json"]);
    cmd.stdin(Stdio::piped());
    let mut child = spawn_run(cmd);
    let _open_stdin = child.stdin.take().unwrap();
    let out = tokio::time::timeout(Duration::from_secs(10), finish_run(child))
        .await
        .expect("headless run must not wait for stdin EOF");
    terminal_json(&out, "completed", 0);
}

#[tokio::test]
async fn plugin_task_template_expands_arguments_and_unknown_template_fails() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/liveplug");
    let plugin = plugin.to_str().unwrap();
    let out = output(sandbox.cmd(&[
        "run",
        "--plugin",
        plugin,
        "--command",
        "liveplug:greet",
        "--args",
        "TEMPLATE_菠萝",
        "--output",
        "json",
    ]))
    .await;
    terminal_json(&out, "completed", 0);
    let request = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let messages = body["messages"].to_string();
    assert!(messages.contains("TEMPLATE_菠萝"), "{messages}");
    assert!(!messages.contains("$ARGUMENTS"), "{messages}");
    for command in ["liveplug:missing", "greet", "missing:greet"] {
        let out = output(sandbox.cmd(&[
            "run",
            "--plugin",
            plugin,
            "--command",
            command,
            "--output",
            "json",
        ]))
        .await;
        terminal_json(&out, "failed", 1);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn resume_across_commands_continues_same_session() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let run = output(sandbox.cmd(&["run", "-t", "say hi", "--output", "json"])).await;
    let first = terminal_json(&run, "completed", 0);
    let id = first["session_id"].as_str().unwrap();
    for resume in [id, "last"] {
        let out =
            output(sandbox.cmd(&["run", "--resume", resume, "-t", "again", "--output", "json"]))
                .await;
        let result = terminal_json(&out, "completed", 0);
        assert_eq!(result["session_id"], id);
        assert_eq!(result["output"], "hi there", "only the current task answer");
    }
    assert_eq!(session_messages(&sandbox, id).len(), 6);
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(
        body["messages"].to_string().contains("say hi"),
        "restored history sent to provider"
    );
    let list = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&list, "sessions list");
    assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 1);
}

#[tokio::test]
async fn resume_preserves_provider_model_and_cwd_and_rejects_conflicting_cwd() {
    let sandbox = Sandbox::new();
    let original_cwd = TempDir::new().unwrap();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "first",
        "--cwd",
        original_cwd.path().to_str().unwrap(),
        "--model",
        "saved-model",
        "--output",
        "json",
    ]))
    .await;
    let first = terminal_json(&out, "completed", 0);
    let id = first["session_id"].as_str().unwrap();
    // Later global defaults and the caller's directory must not redirect a resumed task.
    sandbox.write_config_yaml("provider: nonexistent-provider\nmodel: unrelated-model\n");
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse("pwd > resumed.cwd"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    let out =
        output(sandbox.cmd(&["run", "--resume", id, "-t", "continue", "--output", "json"])).await;
    let resumed = terminal_json(&out, "completed", 0);
    assert_eq!(resumed["session_id"], id);
    let actual = std::fs::read_to_string(original_cwd.path().join("resumed.cwd")).unwrap();
    assert_eq!(
        Path::new(actual.trim()).canonicalize().unwrap(),
        original_cwd.path().canonicalize().unwrap()
    );
    assert!(!sandbox.cwd.path().join("resumed.cwd").exists());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "saved-model");
    }
    let out = output(sandbox.cmd(&[
        "run",
        "--resume",
        id,
        "-t",
        "conflict",
        "--cwd",
        sandbox.cwd.path().to_str().unwrap(),
        "--output",
        "json",
    ]))
    .await;
    let result = terminal_json(&out, "failed", 1);
    assert!(result["error"].as_str().unwrap().contains("--cwd"));
    assert_eq!(result["session_id"], id);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn failed_resumed_run_does_not_return_previous_answer_or_usage() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let out = output(sandbox.cmd(&["run", "-t", "first", "--output", "json"])).await;
    let first = terminal_json(&out, "completed", 0);
    let id = first["session_id"].as_str().unwrap();
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let out =
        output(sandbox.cmd(&["run", "--resume", id, "-t", "second", "--output", "json"])).await;
    let result = terminal_json(&out, "failed", 1);
    assert_eq!(result["session_id"], id);
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());
    assert_eq!(session_messages(&sandbox, id).len(), 3);
}

#[tokio::test]
async fn resume_bad_id_or_absent_last_fails_without_creating_a_session() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_provider("http://127.0.0.1:9");
    for id in ["last", "no-such-session", "../evil"] {
        let out =
            output(sandbox.cmd(&["run", "--resume", id, "-t", "go", "--output", "json"])).await;
        let result = terminal_json(&out, "failed", 1);
        assert!(result["session_id"].is_null());
    }
}

#[tokio::test]
async fn run_does_not_auto_update_preinstalled_plugins() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let metadata = sandbox.agents.path().join("plugins/fakeprov/.install.json");
    let before = serde_json::json!({
        "source":"file:///nonexistent/headless-plugin-repository",
        "commit":"0000000", "installed_at":1,
        "last_update_check":null, "auto_update":true,
    })
    .to_string();
    std::fs::write(&metadata, &before).unwrap();
    let out = output(sandbox.cmd(&["run", "-t", "go", "--output", "json"])).await;
    terminal_json(&out, "completed", 0);
    assert_eq!(
        std::fs::read_to_string(&metadata).unwrap(),
        before,
        "run must not even advance auto-update metadata"
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("auto-update"));
}

#[cfg(unix)]
async fn interrupted_mcp_startup_reaps_process_group(signal_name: Option<&str>) {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let plugin = sandbox.agents.path().join("plugins/slowmcp");
    write_minimal_plugin_source(&plugin, "slowmcp");
    let script = plugin.join("startup.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 120 &\necho $! > \"$0.pid\"\nwait\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        plugin.join("mcp.json"),
        serde_json::json!({
            "mcpServers":{"slow":{"type":"stdio","command":"./startup.sh"}}
        })
        .to_string(),
    )
    .unwrap();
    let timeout = if signal_name.is_some() { "60" } else { "2" };
    let child =
        spawn_run(sandbox.cmd(&["run", "-t", "go", "--output", "json", "--timeout", timeout]));
    let grandchild = wait_pid_file(&plugin.join("startup.sh.pid")).await;
    if let Some(name) = signal_name {
        signal(child.id().unwrap(), name).await;
    }
    let out = tokio::time::timeout(Duration::from_secs(10), finish_run(child))
        .await
        .expect("MCP initialization and cleanup must obey run deadline");
    let (status, code) = if signal_name.is_some() {
        ("cancelled", 130)
    } else {
        ("timed_out", 124)
    };
    let result = terminal_json(&out, status, code);
    assert!(
        result["session_id"].is_null(),
        "initialization has no session yet: {result}"
    );
    eventually_dead(grandchild).await;
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_mcp_initialization_reaps_process_group() {
    interrupted_mcp_startup_reaps_process_group(Some("-TERM")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_during_mcp_initialization_reaps_process_group() {
    interrupted_mcp_startup_reaps_process_group(None).await;
}

/// Cancel while the second MCP server initializes: both the already connected
/// transport and the pending handshake must release their entire child groups.
#[cfg(unix)]
#[tokio::test]
async fn cancelled_multi_server_startup_reaps_connected_and_pending_groups() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let plugin = sandbox.agents.path().join("plugins/multimcp");
    write_minimal_plugin_source(&plugin, "multimcp");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_mcp-fixture-server"),
        plugin.join("fixture-server"),
    )
    .unwrap();
    for (name, tail) in [
        ("ready.sh", "exec \"${PLUGIN_ROOT}/fixture-server\""),
        ("pending.sh", "wait"),
    ] {
        let script = plugin.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 120 &\necho $! > \"$0.pid\"\n{tail}\n",),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        plugin.join("mcp.json"),
        serde_json::json!({
            "mcpServers":{
                "a_ready":{"type":"stdio","command":"./ready.sh"},
                "b_pending":{"type":"stdio","command":"./pending.sh"},
            }
        })
        .to_string(),
    )
    .unwrap();
    let child = spawn_run(sandbox.cmd(&["run", "-t", "go", "--output", "json"]));
    let ready_grandchild = wait_pid_file(&plugin.join("ready.sh.pid")).await;
    // Servers connect in name order. The second process starts only after the
    // fixture server's successful initialize response has been consumed.
    let pending_grandchild = wait_pid_file(&plugin.join("pending.sh.pid")).await;
    signal(ready_grandchild, "-0").await;
    signal(child.id().unwrap(), "-TERM").await;
    let out = tokio::time::timeout(Duration::from_secs(8), finish_run(child))
        .await
        .expect("multi-server startup cancellation must terminate");
    let result = terminal_json(&out, "cancelled", 130);
    assert!(result["session_id"].is_null());
    // Join both assertions so a regression still cleans up both known fixtures.
    let (ready, pending) = tokio::join!(
        tokio::spawn(eventually_dead(ready_grandchild)),
        tokio::spawn(eventually_dead(pending_grandchild)),
    );
    ready.expect("connected server group cleanup");
    pending.expect("pending handshake group cleanup");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_session_end_reports_cancelled_without_completed_answer() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    write_hook_plugin(
        sandbox.agents.path(),
        "slowend",
        "SessionEnd",
        "sleep 120 & echo $! > \"${PLUGIN_ROOT}/grandchild.pid\"; wait",
    );
    let child = spawn_run(sandbox.cmd(&["run", "-t", "go", "--output", "json"]));
    let grandchild =
        wait_pid_file(&sandbox.agents.path().join("plugins/slowend/grandchild.pid")).await;
    signal(child.id().unwrap(), "-TERM").await;
    let out = tokio::time::timeout(Duration::from_secs(8), finish_run(child))
        .await
        .expect("late cancellation cleanup remains bounded");
    let result = terminal_json(&out, "cancelled", 130);
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());
    let messages = session_messages(&sandbox, result["session_id"].as_str().unwrap());
    assert_eq!(
        messages.len(),
        2,
        "completed answer remains in session for recovery"
    );
    assert!(serde_json::to_string(&messages)
        .unwrap()
        .contains("hi there"));
    eventually_dead(grandchild).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// SIGINT/SIGTERM 和任务期限都会终止当前批次、清理孙进程并留下可恢复会话。
#[cfg(unix)]
async fn interrupted_run_reaps_and_resumes(signal_name: Option<&str>) {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(shell_call_sse(
            "sleep 120 & echo $! > grandchild.pid; sleep 120",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    let timeout = if signal_name.is_some() { "60" } else { "2" };
    let child =
        spawn_run(sandbox.cmd(&["run", "-t", "go", "--output", "json", "--timeout", timeout]));
    let pid = child.id().unwrap();
    let grandchild = wait_pid_file(&sandbox.cwd.path().join("grandchild.pid")).await;
    if let Some(name) = signal_name {
        signal(pid, name).await;
    }
    let out = tokio::time::timeout(Duration::from_secs(10), finish_run(child))
        .await
        .expect("interrupted task and cleanup must be bounded");
    let (status, code) = if signal_name.is_some() {
        ("cancelled", 130)
    } else {
        ("timed_out", 124)
    };
    let result = terminal_json(&out, status, code);
    assert_eq!(result["output"], "");
    assert!(result["usage"].is_null());
    eventually_dead(grandchild).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let id = result["session_id"].as_str().unwrap();
    let messages = session_messages(&sandbox, id);
    assert_eq!(
        messages.len(),
        3,
        "task + assistant call + paired cancelled result"
    );
    assert!(messages[2].content.iter().any(|content| matches!(
        content,
        instagent::message::Content::ToolResult { is_error: true, .. }
    )));
    let out =
        output(sandbox.cmd(&["run", "--resume", id, "-t", "again", "--output", "json"])).await;
    let resumed = terminal_json(&out, "completed", 0);
    assert_eq!(resumed["session_id"], id);
    assert_eq!(resumed["output"], "hi there");
    session_messages(&sandbox, id);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_cancels_reaps_and_resumes() {
    interrupted_run_reaps_and_resumes(Some("-INT")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_cancels_reaps_and_resumes() {
    interrupted_run_reaps_and_resumes(Some("-TERM")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_reaps_and_resumes() {
    interrupted_run_reaps_and_resumes(None).await;
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

/// 自动压缩的摘要请求可被取消；未提交摘要时原会话保持可恢复。
#[cfg(unix)]
#[tokio::test]
async fn compact_cancel_mid_summary_keeps_session_usable() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Task Context"))
        .respond_with(sse_body().set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    sandbox.write_config_yaml("provider: fake\nmodel: test-model\ncontext_limit: 10\n");
    let out = output(sandbox.cmd(&["run", "-t", "hello", "--output", "json"])).await;
    let first = terminal_json(&out, "completed", 0);
    let id = first["session_id"].as_str().unwrap();
    let child = spawn_run(sandbox.cmd(&["run", "--resume", id, "-t", "again", "--output", "json"]));
    wait_requests(&server, 2).await;
    signal(child.id().unwrap(), "-INT").await;
    let out = finish_run(child).await;
    let result = terminal_json(&out, "cancelled", 130);
    assert_eq!(result["session_id"], id);
    let messages = session_messages(&sandbox, id);
    assert_eq!(messages.len(), 3, "old exchange + unanswered input");
    let content = serde_json::to_string(&messages).unwrap();
    assert!(
        !content.contains(instagent::message::SUMMARY_PREFIX),
        "cancelled summary must not commit"
    );
    // Restore the ordinary threshold so the next batch tests recovery without another summary.
    sandbox.write_config_yaml("provider: fake\nmodel: test-model\n");
    let out =
        output(sandbox.cmd(&["run", "--resume", id, "-t", "continue", "--output", "json"])).await;
    terminal_json(&out, "completed", 0);
    assert_eq!(session_messages(&sandbox, id).len(), 4);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
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

/// 独立批次恢复历史后自动压缩，再继续处理任务，不需要斜杠命令。
#[tokio::test]
async fn automatic_compaction_then_continue_keeps_session_valid() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());
    sandbox.write_config_yaml("provider: fake\nmodel: test-model\ncontext_limit: 10\n");
    let out = output(sandbox.cmd(&["run", "-t", "hello", "--output", "json"])).await;
    let first = terminal_json(&out, "completed", 0);
    let id = first["session_id"].as_str().unwrap();
    let out = output(sandbox.cmd(&["run", "--resume", id, "-t", "next", "--output", "json"])).await;
    let second = terminal_json(&out, "completed", 0);
    assert_eq!(second["output"], "hi there");
    assert_eq!(second["session_id"], id);
    let messages = session_messages(&sandbox, id);
    let content = serde_json::to_string(&messages).unwrap();
    assert!(
        content.contains(instagent::message::SUMMARY_PREFIX),
        "summary persisted: {content}"
    );
    assert!(content.contains("next"), "new task retained: {content}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "two tasks + automatic summary"
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
