//! repo-improvements 09：CLI 二进制级集成测试。
//!
//! 经 `env!("CARGO_BIN_EXE_instagent")` 起真进程，每测试一套
//! `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` / `INSTAGENT_AGENTS_DIR`
//! 沙箱三变量隔离（tempdir，不改本测试进程 env，不触碰真实目录），
//! provider 用测试进程内的 wiremock SSE 顶替（同 `src/cli/assembly.rs`
//! 进程内测试的做法）。覆盖 README 手工验证清单第 4/5/7/8 条的可自动化部分。

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
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
        .expect(1)
        .mount(server)
        .await;
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

// ---- T1：骨架 —— --help / --version 真进程 ----

#[tokio::test]
async fn help_and_version_run_as_real_process() {
    let sandbox = Sandbox::new();
    let help = output(sandbox.cmd(&["--help"])).await;
    assert_ok(&help, "--help");
    let stdout = String::from_utf8_lossy(&help.stdout);
    for subcommand in ["chat", "run", "sessions", "plugin"] {
        assert!(
            stdout.contains(subcommand),
            "--help 应列出子命令 {subcommand}: {stdout}"
        );
    }

    let version = output(sandbox.cmd(&["--version"])).await;
    assert_ok(&version, "--version");
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(stdout.contains("instagent"), "{stdout}");
}

// ---- T2：run -t 全链路 ----

#[tokio::test]
async fn run_task_full_chain_prints_reply_and_usage() {
    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    mount_chat_completions(&server).await;
    sandbox.install_fake_provider(&server.uri());

    let out = output(sandbox.cmd(&["run", "-t", "say hi"])).await;
    assert_ok(&out, "run -t");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hi there"), "{stdout}");
    assert!(stdout.contains("usage: in=12 out=5"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.lines().any(|line| line.starts_with("session ")),
        "{stderr}"
    );
}

// ---- T3：sessions list / rm ----

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

// ---- T4：plugin install / list / show / disable / enable ----

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

// ---- T5：--plugin PATH 临时加载（开发插件目录内联 provider）----

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
    assert!(stdout.contains("hi there"), "{stdout}");
    assert!(stdout.contains("usage: in=12 out=5"), "{stdout}");
}
