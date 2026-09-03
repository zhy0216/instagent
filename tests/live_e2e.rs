//! live-e2e：真模型（qwen3.6-flash，token-plan 网关）端到端集成测试。
//!
//! Sandbox 骨架复制自 `tests/cli_e2e.rs`：tempdir 三变量隔离 +
//! `env!("CARGO_BIN_EXE_instagent")` 真进程 + 进程组/`kill_on_drop`/整体超时
//! 兜底；真 API 比 wiremock 慢一个量级，单测试超时放宽到 180s。每个测试
//! 开头检查 `TOKEN_PLAN_API_KEY`：缺失则打 skip 后返回，离线 `cargo test`
//! 依旧全绿。断言只查随机标记词（MANGO_77 这类）与结构标记（`usage:`、
//! `session `、`▶`），prompt 命令式写死，从不查自然语言措辞（防 flake）。
//!
//! 用例表对应 `plans/live-e2e/plan.md`：a1/a2 run 全链路，b1/b2 会话管理，
//! c1/c2 模式与审批白名单，d1–d5 插件链路（command tool / hooks / skill /
//! 斜杠命令，消费 `tests/fixtures/liveplug`），e1 环境变量覆盖。

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

const BIN: &str = env!("CARGO_BIN_EXE_instagent");
/// 真 API 比 wiremock 慢一个量级；超时兜底防挂死。
const TIMEOUT: Duration = Duration::from_secs(180);

const PLUGIN_SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const LIVE_BASE_URL: &str = "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";
const DEFAULT_LIVE_MODEL: &str = "qwen3.6-flash";

/// 真模型门控：每个测试开头先过这一关。
fn has_key() -> bool {
    if std::env::var("TOKEN_PLAN_API_KEY").is_ok() {
        true
    } else {
        eprintln!("skip: TOKEN_PLAN_API_KEY not set");
        false
    }
}

/// 默认 `qwen3.6-flash`，可被 `INSTAGENT_LIVE_MODEL` 覆盖。
fn live_model() -> String {
    std::env::var("INSTAGENT_LIVE_MODEL").unwrap_or_else(|_| DEFAULT_LIVE_MODEL.to_string())
}

fn live_config_yaml(model: &str, extra: &str) -> String {
    format!("provider: live\nmodel: {model}\nmax_tokens: 1024\n{extra}")
}

fn liveplug_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/liveplug")
}

/// 每测试一个沙箱：三变量目录 + 独立 cwd，全部 tempdir（同 `cli_e2e.rs`）。
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
    /// 清掉外部可能泄漏进来的覆盖变量（`TOKEN_PLAN_API_KEY` 随环境继承）。
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
            .env_remove("INSTAGENT_MODE")
            .env_remove("RUST_LOG");
        cmd
    }

    fn write_config_yaml(&self, yaml: &str) {
        std::fs::write(self.config.path().join("config.yaml"), yaml).unwrap();
    }

    /// 用户插件 `liveprov`：provider `live`（openai 引擎）指向 token-plan
    /// 网关，密钥走 `TOKEN_PLAN_API_KEY` 环境变量；config.yaml 选它。
    fn install_live_provider(&self) {
        let dir = self.agents.path().join("plugins").join("liveprov");
        let providers = dir.join("dev.instagent").join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"liveprov","version":"1.0.0"}}"#),
        )
        .unwrap();
        std::fs::write(
            providers.join("live.json"),
            format!(
                r#"{{"name":"live","engine":"openai","api_key_env":"TOKEN_PLAN_API_KEY","base_url":"{LIVE_BASE_URL}","timeout_seconds":120}}"#
            ),
        )
        .unwrap();
        self.write_config_yaml(&live_config_yaml(&live_model(), ""));
    }

    /// 预信任 `liveplug`（§5.2：可执行组件不走交互确认）。
    fn trust_liveplug(&self) {
        std::fs::write(
            self.config.path().join("settings.json"),
            r#"{"trustedPlugins":["liveplug"]}"#,
        )
        .unwrap();
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
        .expect("instagent 二进制执行超时（180s）")
        .expect("spawn instagent 二进制")
}

/// 同 [`output`]，但先经 stdin 喂一段输入（chat REPL 管道输入用）。
async fn output_with_stdin(mut cmd: tokio::process::Command, stdin: &[u8]) -> Output {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    instagent::subprocess::configure_subprocess(&mut cmd);
    let mut child = cmd.spawn().expect("spawn instagent 二进制");
    let mut child_stdin = child.stdin.take().unwrap();
    child_stdin
        .write_all(stdin)
        .await
        .expect("write stdin to instagent");
    drop(child_stdin);
    tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .expect("instagent 二进制执行超时（180s）")
        .expect("wait instagent 二进制")
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

/// run 的 stderr 里取 `session <id>` 行（§3/§7）。
fn session_id_of(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .expect("run 应在 stderr 打出 session id")
        .trim()
        .to_string()
}

const SHELL_ECHO_MANGO: &str = "用 shell 工具执行命令 echo MANGO_77，并把命令输出原样报告。";
const REPLY_OK_ONLY: &str = "只回复两个字母 OK，不要输出任何其他内容。";

// ---- T2：核心链路 a1 / a2 / b1 / b2 / c1 / c2 / e1 ----

/// a1：`run -t` 最简一轮——回复 + `usage:`（stdout）+ `session <id>`（stderr）。
#[tokio::test]
async fn live_a1_run_simple_reply() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let out = output(sandbox.cmd(&["run", "-t", REPLY_OK_ONLY])).await;
    assert_ok(&out, "a1 run -t");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK"), "{stdout}");
    assert!(stdout.contains("usage:"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.lines().any(|line| line.starts_with("session ")),
        "{stderr}"
    );
}

/// a2：auto 模式真工具调用——shell 执行 echo，输出链含标记词。
#[tokio::test]
async fn live_a2_run_shell_tool() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let out = output(sandbox.cmd(&["run", "-t", SHELL_ECHO_MANGO])).await;
    assert_ok(&out, "a2 run -t shell");
    // `▶`/预览/回复都经 render.rs 打到 stdout（run 只有 session/notes 走 stderr）。
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("▶ shell"), "{stdout}");
    assert!(stdout.contains("MANGO_77"), "{stdout}");
}

/// b1：a2 产生会话后——`sessions list` / jsonl header / `sessions rm`。
#[tokio::test]
async fn live_b1_sessions_list_read_rm() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let model = live_model();

    let run = output(sandbox.cmd(&["run", "-t", SHELL_ECHO_MANGO])).await;
    assert_ok(&run, "b1 run -t");
    let id = session_id_of(&String::from_utf8_lossy(&run.stderr));
    let file = sandbox.sessions_dir().join(format!("{id}.jsonl"));
    assert!(file.is_file(), "会话文件应落在沙箱数据目录: {file:?}");

    let list = output(sandbox.cmd(&["sessions", "list"])).await;
    assert_ok(&list, "sessions list");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains(&id), "{stdout}");
    assert!(stdout.contains(&format!("live/{model}")), "{stdout}");

    // jsonl 首行 header：id/created/cwd/provider/model 字段齐（§7）。
    let first = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let header: serde_json::Value = serde_json::from_str(&first).expect("header 是合法 JSON");
    assert_eq!(header["id"], id, "{first}");
    assert_eq!(header["provider"], "live", "{first}");
    assert_eq!(header["model"], model, "{first}");
    assert!(header.get("created").is_some(), "{first}");
    assert!(header.get("cwd").is_some(), "{first}");

    let rm = output(sandbox.cmd(&["sessions", "rm", &id])).await;
    assert_ok(&rm, "sessions rm");
    assert!(
        String::from_utf8_lossy(&rm.stdout).contains(&format!("removed session {id}")),
        "{}",
        String::from_utf8_lossy(&rm.stdout)
    );
    assert!(!file.exists(), "rm 后会话文件应被删除");
}

/// b2：管道 stdin 喂 chat 记暗号，`--resume last` 问回来。
#[tokio::test]
async fn live_b2_chat_remember_resume() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let first = output_with_stdin(
        sandbox.cmd(&["chat"]),
        "记住暗号 BLUE_OTTER_3。只回复“记住了”，不要输出其他内容。\n/exit\n".as_bytes(),
    )
    .await;
    assert_ok(&first, "b2 chat 记暗号");

    let second = output_with_stdin(
        sandbox.cmd(&["chat", "--resume", "last"]),
        "我刚才让你记住的暗号是什么？只回复暗号本身，不要输出其他内容。\n/exit\n".as_bytes(),
    )
    .await;
    assert_ok(&second, "b2 chat --resume last 问暗号");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("BLUE_OTTER_3"), "{stdout}");
}

/// c1：`--mode chat` 不给模型工具——请求用 shell 也不会出现 `▶`。
#[tokio::test]
async fn live_c1_chat_mode_has_no_tools() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let out = output(sandbox.cmd(&[
        "run",
        "--mode",
        "chat",
        "-t",
        "用 shell 工具执行命令 echo HI_42，并把命令输出原样报告。",
    ]))
    .await;
    assert_ok(&out, "c1 run --mode chat");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("▶"),
        "chat 模式不应有任何工具调用: {stdout}"
    );
}

/// c2：approve 白名单——无 `always_allow` 时 shell 被拒，配上后放行。
#[tokio::test]
async fn live_c2_approve_whitelist() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    // run 非交互：approve 模式下未命中白名单的调用直接拒绝（无 `▶ shell`）。
    let denied = output(sandbox.cmd(&["run", "--mode", "approve", "-t", SHELL_ECHO_MANGO])).await;
    assert_ok(&denied, "c2 approve 无白名单");
    let stdout = String::from_utf8_lossy(&denied.stdout);
    assert!(
        !stdout.contains("▶ shell"),
        "未配白名单时 shell 应被拒: {stdout}"
    );

    // 配 `always_allow: [shell]` 后同一任务放行。
    sandbox.write_config_yaml(&live_config_yaml(
        &live_model(),
        "always_allow:\n  - shell\n",
    ));
    let allowed = output(sandbox.cmd(&["run", "--mode", "approve", "-t", SHELL_ECHO_MANGO])).await;
    assert_ok(&allowed, "c2 approve 配白名单");
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(stdout.contains("▶ shell"), "{stdout}");
    assert!(stdout.contains("MANGO_77"), "{stdout}");
}

/// e1：`INSTAGENT_MODEL` 覆盖 config 里故意写错的模型名（§4.2）。
#[tokio::test]
async fn live_e1_env_model_override() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.write_config_yaml(&live_config_yaml("no-such-model-e1", ""));

    let mut cmd = sandbox.cmd(&["run", "-t", REPLY_OK_ONLY]);
    cmd.env("INSTAGENT_MODEL", live_model());
    let out = output(cmd).await;
    assert_ok(&out, "e1 INSTAGENT_MODEL 覆盖");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("usage:"), "{stdout}");
}

// ---- T3：插件链路 d1–d5（夹具 tests/fixtures/liveplug 由 01 产出）----

/// d1：command tool——`liveplug__echoer` 回显标记词（工具名拼法 §9.4）。
#[tokio::test]
async fn live_d1_plugin_command_tool() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.trust_liveplug();
    let plugin = liveplug_fixture().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "调用工具 liveplug__echoer，参数 text 设为 MANGO_77，并把工具返回的内容原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d1 command tool");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("▶ liveplug__echoer"), "{stdout}");
    assert!(stdout.contains("MANGO_77"), "{stdout}");
}

/// d2：PreToolUse guard exit 2 阻止——模型收到 is_error 后继续完成不挂死。
#[tokio::test]
async fn live_d2_plugin_hook_blocks_shell() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.trust_liveplug();
    let plugin = liveplug_fixture().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "用 shell 工具执行命令 echo forbidden_marker，并把命令输出原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d2 hook block");
    // 被阻止的调用不 emit ToolStart，只有 is_error 的 ToolDone 预览（§9.5）。
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("blocked by PreToolUse hook"), "{stdout}");
}

/// d3：hooks 载荷落盘——标记文件存在且 PostToolUse 载荷含 `tool_name`。
///
/// hooks 环境白名单不含 `PLUGIN_DATA`（src/hooks.rs），01 的脚本落盘
/// `${PLUGIN_ROOT}/.hook-out/`，即夹具目录下。
#[tokio::test]
async fn live_d3_plugin_hook_payloads() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.trust_liveplug();
    let plugin = liveplug_fixture().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "用 shell 工具执行命令 echo HOOK_PROBE_31，并把命令输出原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d3 hook payloads");

    let hook_out = liveplug_fixture().join(".hook-out");
    let session_start = hook_out.join("session_start.json");
    let post_tool_use = hook_out.join("post_tool_use.json");
    assert!(
        session_start.is_file(),
        "SessionStart 载荷应落盘: {session_start:?}"
    );
    let post = std::fs::read_to_string(&post_tool_use)
        .unwrap_or_else(|e| panic!("PostToolUse 载荷应落盘 {post_tool_use:?}: {e}"));
    assert!(post.contains("tool_name"), "{post}");
    // 夹具目录保持干净（并发测试会再写，各自脚本自带 mkdir -p）。
    let _ = std::fs::remove_dir_all(&hook_out);
}

/// d4：skill——问暗号，模型经 skill 索引 + `load_skill` 回复 KIWI_55（§9.6）。
#[tokio::test]
async fn live_d4_plugin_skill() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.trust_liveplug();
    let plugin = liveplug_fixture().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "请问暗号（passphrase）是什么？只回复暗号本身，不要输出其他内容。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d4 skill");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("KIWI_55"), "{stdout}");
}

/// d5：斜杠命令——`/greet pineapple` 展开 `$ARGUMENTS`，回复提及该词（§9.7）。
#[tokio::test]
async fn live_d5_plugin_slash_command() {
    if !has_key() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.trust_liveplug();
    let plugin = liveplug_fixture().display().to_string();

    let out = output_with_stdin(
        sandbox.cmd(&["chat", "--plugin", &plugin]),
        b"/greet pineapple\n/exit\n",
    )
    .await;
    assert_ok(&out, "d5 slash command");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pineapple"), "{stdout}");
}
