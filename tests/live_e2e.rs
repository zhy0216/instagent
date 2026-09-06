//! live-e2e：真模型（qwen3.6-flash，token-plan 网关）端到端集成测试。
//!
//! Sandbox 骨架复制自 `tests/cli_e2e.rs`：tempdir 三变量隔离 +
//! `env!("CARGO_BIN_EXE_instagent")` 真进程 + 进程组/`kill_on_drop`/整体超时
//! 兜底；真 API 比 wiremock 慢一个量级，单测试超时放宽到 180s。10 个在线
//! 用例均为 ignored，显式运行：`cargo test --test live_e2e -- --ignored`。
//! 显式运行要求非空 `TOKEN_PLAN_API_KEY`；普通测试只验证离线夹具隔离。
//! 在线断言只查随机标记词（MANGO_77 这类）与结构标记（`usage:`、
//! `session `、`▶`），prompt 命令式写死，从不查自然语言措辞（防 flake）。
//! 结构标记按 ADR 0003 D4 契约定位：答案文本在 stdout，`usage:` /
//! `session ` / `▶` 等诊断全在 stderr。
//!
//! 用例表对应 `plans/live-e2e/plan.md`：a1/a2 run 全链路，b1/b2 会话管理，
//! d1–d5 插件链路（command tool / hooks / skill / 任务模板，消费
//! `tests/fixtures/liveplug`），e1 环境变量覆盖。

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use instagent::hooks::{HookContext, HookDecision, HookEvent, Hooks};
use instagent::plugin::{manifest::read_manifest, Plugin, PluginSet, PluginSource};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_instagent");
/// 真 API 比 wiremock 慢一个量级；超时兜底防挂死。
const TIMEOUT: Duration = Duration::from_secs(180);

const PLUGIN_SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const LIVE_BASE_URL: &str = "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";
const DEFAULT_LIVE_MODEL: &str = "qwen3.6-flash";

/// 显式请求在线测试时，缺失、空串和纯空白凭据必须失败，不能假报通过。
fn require_key() {
    assert!(
        std::env::var("TOKEN_PLAN_API_KEY").is_ok_and(|key| !key.trim().is_empty()),
        "TOKEN_PLAN_API_KEY must be set to a non-empty value for explicitly requested live tests"
    );
}

/// 默认 `qwen3.6-flash`，可被 `INSTAGENT_LIVE_MODEL` 覆盖。
fn live_model() -> String {
    std::env::var("INSTAGENT_LIVE_MODEL").unwrap_or_else(|_| DEFAULT_LIVE_MODEL.to_string())
}

fn live_config_yaml(model: &str, extra: &str) -> String {
    format!("provider: live\nmodel: {model}\nmax_tokens: 1024\n{extra}")
}

/// 只复制版本化输入；明确排除 `.hook-out`、日志、缓存等所有运行输出。
/// 增加夹具输入时同步此清单，不递归复制整个源码目录。
const LIVEPLUG_FILES: &[&str] = &[
    "plugin.json",
    "dev.instagent/commands/greet.md",
    "dev.instagent/hooks.json",
    "dev.instagent/tools/echoer.json",
    "scripts/echoer.sh",
    "scripts/guard.sh",
    "scripts/post_tool_use.sh",
    "scripts/session_start.sh",
    "skills/secret/SKILL.md",
];

fn liveplug_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/liveplug")
}

fn copy_liveplug_fixture(source: &Path, destination: &Path) {
    for relative in LIVEPLUG_FILES {
        let target = destination.join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // fs::copy 复制字节和权限，保留脚本执行位；不用共享文件或硬链接。
        std::fs::copy(source.join(relative), target).unwrap();
    }
}

/// 每测试一个沙箱：三变量目录 + 独立 cwd + 私有 liveplug，全部 tempdir。
struct Sandbox {
    config: TempDir,
    data: TempDir,
    agents: TempDir,
    cwd: TempDir,
    liveplug: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let liveplug = TempDir::new().unwrap();
        copy_liveplug_fixture(&liveplug_source(), liveplug.path());
        Self {
            config: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
            agents: TempDir::new().unwrap(),
            cwd: TempDir::new().unwrap(),
            liveplug,
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

// ---- 离线夹具回归：不创建 provider，不读取凭据，不访问模型 ----

fn fixture_snapshot(root: &Path) -> Vec<(Vec<u8>, std::fs::Permissions)> {
    LIVEPLUG_FILES
        .iter()
        .map(|relative| {
            let path = root.join(relative);
            (
                std::fs::read(&path).unwrap(),
                std::fs::metadata(path).unwrap().permissions(),
            )
        })
        .collect()
}

fn fixture_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let relative = PathBuf::from(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            paths.extend(
                fixture_paths(&entry.path())
                    .into_iter()
                    .map(|path| relative.join(path)),
            );
        }
        paths.push(relative);
    }
    paths.sort();
    paths
}

fn fixture_hooks(sandbox: &Sandbox) -> Hooks {
    let root = sandbox.liveplug.path();
    let mut plugins = PluginSet::default();
    plugins.plugins.push(Plugin {
        manifest: read_manifest(root).unwrap(),
        root: root.to_path_buf(),
        source: PluginSource::Cli,
    });
    Hooks::load(&plugins).unwrap()
}

fn assert_hook_payload(root: &Path, filename: &str, context: &HookContext) {
    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".hook-out").join(filename)).unwrap())
            .unwrap();
    assert_eq!(payload, serde_json::to_value(context).unwrap());
}

#[tokio::test]
async fn liveplug_sandboxes_isolate_hooks_and_cleanup() {
    let source = liveplug_source();
    let source_snapshot = fixture_snapshot(&source);
    let source_paths = fixture_paths(&source);
    let first = Sandbox::new();
    let second = Sandbox::new();
    let first_root = first.liveplug.path().to_path_buf();
    let second_root = second.liveplug.path().to_path_buf();
    assert_ne!(first_root, second_root);
    assert_ne!(first_root, source);
    assert_ne!(second_root, source);
    assert_eq!(fixture_snapshot(&first_root), source_snapshot);
    assert_eq!(fixture_snapshot(&second_root), source_snapshot);
    assert!(!first_root.join(".hook-out").exists());
    assert!(!second_root.join(".hook-out").exists());

    // 加载真实 hooks.json 并执行有执行位的真实脚本；Hooks::run 内部使用
    // ProcessGroupChild（进程组 + kill_on_drop），无模型参与。
    let first_hooks = fixture_hooks(&first);
    let second_hooks = fixture_hooks(&second);
    for (event, filename) in [
        (HookEvent::SessionStart, "session_start.json"),
        (HookEvent::PostToolUse, "post_tool_use.json"),
    ] {
        let first_context = HookContext::new(event, "FIRST_SANDBOX_31")
            .with_tool(
                "shell",
                Some(serde_json::json!({"command": "echo FIRST_31"})),
            )
            .with_working_dir(first.cwd.path());
        let second_context = HookContext::new(event, "SECOND_SANDBOX_77")
            .with_tool(
                "shell",
                Some(serde_json::json!({"command": "echo SECOND_77"})),
            )
            .with_working_dir(second.cwd.path());
        let (first_decision, second_decision) = tokio::join!(
            first_hooks.run(&first_context),
            second_hooks.run(&second_context)
        );
        assert_eq!(first_decision, HookDecision::Allow);
        assert_eq!(second_decision, HookDecision::Allow);
        // 两边都运行完才检查完整载荷，避免先读后覆盖掩盖串写。
        assert_hook_payload(&first_root, filename, &first_context);
        assert_hook_payload(&second_root, filename, &second_context);
    }

    // 副本也不能通过硬链接共享输入文件。
    std::fs::write(first_root.join("plugin.json"), "{}").unwrap();
    assert_eq!(fixture_snapshot(&second_root), source_snapshot);
    let second_post = std::fs::read(second_root.join(".hook-out/post_tool_use.json")).unwrap();
    drop(first);
    assert!(
        !first_root.exists(),
        "TempDir 应清理本 Sandbox 的夹具和输出"
    );
    assert_eq!(
        std::fs::read(second_root.join(".hook-out/post_tool_use.json")).unwrap(),
        second_post,
        "销毁另一个 Sandbox 不得删除或改写存活 Sandbox 的输出"
    );

    // 另一个 Sandbox 销毁后，存活副本仍能运行写入 hook 和 guard。
    let context = HookContext::new(HookEvent::SessionStart, "SECOND_STILL_ALIVE")
        .with_working_dir(second.cwd.path());
    assert_eq!(second_hooks.run(&context).await, HookDecision::Allow);
    assert_hook_payload(&second_root, "session_start.json", &context);
    let guard_context = HookContext::new(HookEvent::PreToolUse, "SECOND_STILL_ALIVE")
        .with_tool(
            "shell",
            Some(serde_json::json!({"command": "echo forbidden_marker"})),
        )
        .with_working_dir(second.cwd.path());
    assert!(matches!(
        second_hooks.run(&guard_context).await,
        HookDecision::Block(reason) if reason.contains("blocked by liveplug guard")
    ));
    drop(second);
    assert!(!second_root.exists());
    assert_eq!(fixture_snapshot(&source), source_snapshot);
    assert_eq!(fixture_paths(&source), source_paths, "源码夹具不得新增输出");
}

#[test]
fn liveplug_copy_excludes_runtime_outputs() {
    // 在临时副本模拟旧输出；源码目录（包括已有 .hook-out）始终只读。
    let source = Sandbox::new();
    let inputs = fixture_snapshot(source.liveplug.path());
    let input_paths = fixture_paths(source.liveplug.path());
    let outputs = [
        ".hook-out/session_start.json",
        "run.log",
        "scripts/debug.log",
        "dev.instagent/output.json",
        "skills/secret/.cache/run.txt",
    ];
    for relative in outputs {
        let path = source.liveplug.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "OLD_OUTPUT_19").unwrap();
    }
    let destination = TempDir::new().unwrap();
    copy_liveplug_fixture(source.liveplug.path(), destination.path());
    assert_eq!(fixture_snapshot(destination.path()), inputs);
    assert_eq!(fixture_paths(destination.path()), input_paths);
    for relative in outputs {
        assert_eq!(
            std::fs::read_to_string(source.liveplug.path().join(relative)).unwrap(),
            "OLD_OUTPUT_19",
            "复制不得清理输入目录中的既有输出"
        );
    }
}

const SHELL_ECHO_MANGO: &str = "用 shell 工具执行命令 echo MANGO_77，并把命令输出原样报告。";
const REPLY_OK_ONLY: &str = "只回复两个字母 OK，不要输出任何其他内容。";

// ---- T2：核心链路 a1 / a2 / b1 / b2 / e1 ----

/// a1：`run -t` 最简一轮——回复（stdout）+ `usage:`（stderr）+ `session <id>`（stderr）。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_a1_run_simple_reply() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let out = output(sandbox.cmd(&["run", "-t", REPLY_OK_ONLY])).await;
    assert_ok(&out, "a1 run -t");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage:"), "{stderr}");
    assert!(
        stderr.lines().any(|line| line.starts_with("session ")),
        "{stderr}"
    );
}

/// a2：auto 模式真工具调用——shell 执行 echo，输出链含标记词。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_a2_run_shell_tool() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let out = output(sandbox.cmd(&["run", "-t", SHELL_ECHO_MANGO])).await;
    assert_ok(&out, "a2 run -t shell");
    // D4：工具事件 `▶` / 预览走 stderr，最终回复（答案文本）走 stdout。
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("▶ shell"), "{stderr}");
    // 标记词在工具预览（stderr）必现；回复（stdout）通常也复述，合查防 flake。
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr);
    assert!(combined.contains("MANGO_77"), "{combined}");
}

/// b1：a2 产生会话后——`sessions list` / jsonl header / `sessions rm`。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_b1_sessions_list_read_rm() {
    require_key();
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

/// b2：独立批次记住暗号，`run --resume last` 恢复后读回。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_b2_run_remember_resume() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();

    let first = output(sandbox.cmd(&[
        "run",
        "-t",
        "记住暗号 BLUE_OTTER_3。只回复“记住了”，不要输出其他内容。",
    ]))
    .await;
    assert_ok(&first, "b2 run 记暗号");

    let second = output(sandbox.cmd(&[
        "run",
        "--resume",
        "last",
        "-t",
        "我刚才让你记住的暗号是什么？只回复暗号本身，不要输出其他内容。",
    ]))
    .await;
    assert_ok(&second, "b2 run --resume last 问暗号");
    assert_eq!(
        session_id_of(&String::from_utf8_lossy(&first.stderr)),
        session_id_of(&String::from_utf8_lossy(&second.stderr)),
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("BLUE_OTTER_3"), "{stdout}");
}

/// e1：`INSTAGENT_MODEL` 覆盖 config 里故意写错的模型名（§4.2）。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_e1_env_model_override() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    sandbox.write_config_yaml(&live_config_yaml("no-such-model-e1", ""));

    let mut cmd = sandbox.cmd(&["run", "-t", REPLY_OK_ONLY]);
    cmd.env("INSTAGENT_MODEL", live_model());
    let out = output(cmd).await;
    assert_ok(&out, "e1 INSTAGENT_MODEL 覆盖");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- T3：插件链路 d1–d5（夹具 tests/fixtures/liveplug 由 01 产出）----

/// d1：command tool——`liveplug__echoer` 回显标记词（工具名拼法 §9.4）。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_d1_plugin_command_tool() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let plugin = sandbox.liveplug.path().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "调用工具 liveplug__echoer，参数 text 设为 MANGO_77，并把工具返回的内容原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d1 command tool");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("▶ liveplug__echoer"), "{stderr}");
    // 标记词在工具预览（stderr）必现；回复（stdout）通常也复述，合查防 flake。
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr);
    assert!(combined.contains("MANGO_77"), "{combined}");
}

/// d2：PreToolUse guard exit 2 阻止——模型收到 is_error 后继续完成不挂死。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_d2_plugin_hook_blocks_shell() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let plugin = sandbox.liveplug.path().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "用 shell 工具执行命令 echo forbidden_marker，并把命令输出原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d2 hook block");
    // 被阻止的调用不 emit ToolStart，只有 is_error 的 ToolDone 预览（§9.5）；
    // D4 下预览走 stderr。
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("blocked by PreToolUse hook"), "{stderr}");
}

/// d3：hooks 载荷落盘——标记文件存在且 PostToolUse 载荷含 `tool_name`。
///
/// hooks 环境白名单不含 `PLUGIN_DATA`（src/hooks.rs），01 的脚本落盘
/// `${PLUGIN_ROOT}/.hook-out/`，即当前 Sandbox 的私有夹具目录下。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_d3_plugin_hook_payloads() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let plugin = sandbox.liveplug.path().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "-t",
        "用 shell 工具执行命令 echo HOOK_PROBE_31，并把命令输出原样报告。",
        "--plugin",
        &plugin,
    ]))
    .await;
    assert_ok(&out, "d3 hook payloads");

    let hook_out = sandbox.liveplug.path().join(".hook-out");
    let session_start = hook_out.join("session_start.json");
    let post_tool_use = hook_out.join("post_tool_use.json");
    assert!(
        session_start.is_file(),
        "SessionStart 载荷应落盘: {session_start:?}"
    );
    let post = std::fs::read_to_string(&post_tool_use)
        .unwrap_or_else(|e| panic!("PostToolUse 载荷应落盘 {post_tool_use:?}: {e}"));
    assert!(post.contains("tool_name"), "{post}");
}

/// d4：skill——问暗号，模型经 skill 索引 + `load_skill` 回复 KIWI_55（§9.6）。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_d4_plugin_skill() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let plugin = sandbox.liveplug.path().display().to_string();

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

/// d5：任务模板展开 `$ARGUMENTS`，回复提及该词（§9.7）。
#[tokio::test]
#[ignore = "requires TOKEN_PLAN_API_KEY; run explicitly with --ignored"]
async fn live_d5_plugin_task_template() {
    require_key();
    let sandbox = Sandbox::new();
    sandbox.install_live_provider();
    let plugin = sandbox.liveplug.path().display().to_string();

    let out = output(sandbox.cmd(&[
        "run",
        "--plugin",
        &plugin,
        "--command",
        "liveplug:greet",
        "--args",
        "pineapple",
    ]))
    .await;
    assert_ok(&out, "d5 task template");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pineapple"), "{stdout}");
}
