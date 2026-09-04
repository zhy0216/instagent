//! `CommandTools`：`dev.instagent/tools/*.json`，脚本即工具（第三版 §2.9）。
//!
//! 每个启用插件一个实例（id = `cmd:<plugin>`），工具名 `<plugin>__<tool>`
//! （前缀在 [`ToolSource::list`] 里拼好，Registry 只做模型可见名映射）。
//! 执行：命令原样交给 `sh -c`，`${PLUGIN_ROOT}` 经 `PLUGIN_ROOT` 环境变量
//! 传递、由 shell 自行展开（不替换进命令字符串，ADR 0003 D2 / S11）；
//! 环境走插件 baseline + manifest allowlist（[`crate::hooks::apply_plugin_env`]），
//! 默认不泄露 provider credentials / session secrets。input JSON 写 stdin，
//! stdout 作为工具结果，退出码非 0 / 超时 / 取消 / 输出超限即 `is_error`；
//! 子进程走 `03` 的进程组（[`ProcessGroupChild`]，超限 / 超时 / 取消 kill 整组），
//! 输出走 `crate::subprocess::run_bounded` 有界收集。工具定义 JSON 读取有
//! [`MAX_TOOL_DEF_BYTES`] 上限，超限跳过并在诊断里带来源路径。
//! 解析失败或非法的 tool JSON 跳过不报错（warn 日志）。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use crate::plugin::Plugin;
use crate::plugin::PluginSet;
use crate::plugin::NAMESPACE;
use crate::subprocess::run_bounded;
use crate::subprocess::write_stdin;
use crate::subprocess::BoundedOutput;
use crate::subprocess::Outcome;
use crate::subprocess::ProcessGroupChild;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;
use crate::tools::NAME_SEP;

/// `timeout_secs` 缺省值。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 每路输出收集硬上限（`run_bounded`）：超限杀整个进程组、保留截断头部与
/// `BoundedOutput::truncation_note` 摘要，输出绝不无界进内存（R3 / todo 06）。
pub const OUTPUT_CAP_BYTES: usize = 1024 * 1024;
/// 单个工具定义 JSON（含 input_schema）的读取上限：超限跳过，诊断带来源路径。
pub const MAX_TOOL_DEF_BYTES: u64 = 64 * 1024;

/// 单个 command tool 定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// 可执行命令；`${PLUGIN_ROOT}` 原样保留，运行时经 `PLUGIN_ROOT` 环境变量
    /// 由 shell 展开（ADR 0003 D2）。
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug)]
pub struct CommandTools {
    /// `cmd:<plugin>`。
    pub id: String,
    /// 插件根目录；运行时经 `PLUGIN_ROOT` 环境变量传给子进程（相对命令的基准目录）。
    pub plugin_root: PathBuf,
    /// manifest `extensions[dev.instagent].env` 声明的环境变量名（ADR 0003 D2 allowlist）。
    pub declared_env: Vec<String>,
    pub tools: Vec<CommandToolDef>,
}

impl CommandTools {
    /// 每个启用插件解析出一个实例（无 tools 目录或无有效工具的插件跳过）。
    pub fn load(plugins: &PluginSet) -> crate::Result<Vec<CommandTools>> {
        let mut out = Vec::new();
        for plugin in plugins.iter() {
            if let Some(instance) = load_plugin(plugin)? {
                out.push(instance);
            }
        }
        Ok(out)
    }

    fn plugin(&self) -> &str {
        self.id.strip_prefix("cmd:").unwrap_or(&self.id)
    }

    /// Registry 路由用的真名：`<plugin>__<tool>`。
    fn visible_name(&self, tool: &str) -> String {
        format!("{}{NAME_SEP}{tool}", self.plugin())
    }

    fn find(&self, name: &str) -> Option<&CommandToolDef> {
        self.tools
            .iter()
            .find(|def| self.visible_name(&def.name) == name)
    }
}

fn load_plugin(plugin: &Plugin) -> crate::Result<Option<CommandTools>> {
    let dir = plugin.root.join(NAMESPACE).join("tools");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            tracing::warn!("跳过 {}：读取目录失败 {err}", dir.display());
            return Ok(None);
        }
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();

    let mut tools: Vec<CommandToolDef> = Vec::new();
    for path in files {
        let def = match parse_tool_file(&path) {
            Ok(def) => def,
            Err(err) => {
                tracing::warn!("跳过 {}: {err:#}", path.display());
                continue;
            }
        };
        if tools.iter().any(|existing| existing.name == def.name) {
            tracing::warn!("跳过 {}：工具名 `{}` 重复", path.display(), def.name);
            continue;
        }
        tools.push(def);
    }
    if tools.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommandTools {
        id: format!("cmd:{}", plugin.manifest.name),
        plugin_root: plugin.root.clone(),
        declared_env: crate::hooks::declared_env(plugin),
        tools,
    }))
}

/// 读单个工具定义：字节数封顶 [`MAX_TOOL_DEF_BYTES`]，超限报错（由
/// [`load_plugin`] 带路径跳过），防止巨大 / 恶意 JSON 无界进内存。
fn parse_tool_file(path: &Path) -> crate::Result<CommandToolDef> {
    use std::io::Read;
    let mut text = String::new();
    let read = std::fs::File::open(path)?
        .take(MAX_TOOL_DEF_BYTES + 1)
        .read_to_string(&mut text)?;
    if read as u64 > MAX_TOOL_DEF_BYTES {
        anyhow::bail!("tool definition exceeds {MAX_TOOL_DEF_BYTES} bytes");
    }
    let def: CommandToolDef = serde_json::from_str(&text)?;
    if def.name.is_empty() || def.name.contains(NAME_SEP) || def.name.contains(':') {
        anyhow::bail!("invalid tool name `{}`", def.name);
    }
    if def.command.trim().is_empty() {
        anyhow::bail!("tool `{}` has an empty command", def.name);
    }
    Ok(def)
}

#[async_trait]
impl ToolSource for CommandTools {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|def| ToolSpec {
                name: self.visible_name(&def.name),
                description: def.description.clone(),
                input_schema: def.input_schema.clone(),
                read_only: def.read_only,
            })
            .collect()
    }

    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(def) = self.find(name) else {
            return ToolOutput::err(format!("unknown tool: {name}"));
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&def.command);
        cmd.current_dir(&ctx.cwd);
        crate::hooks::apply_plugin_env(&mut cmd, &self.plugin_root, &self.declared_env);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = match ProcessGroupChild::spawn(&mut cmd) {
            Ok(child) => child,
            Err(err) => {
                return ToolOutput::err(format!("Failed to spawn `{}`: {err}", def.command));
            }
        };

        // input JSON 写 stdin（后台写，防大输入死锁；脚本不读 stdin 时 BrokenPipe 忽略）。
        let payload = serde_json::to_vec(&input).unwrap_or_else(|_| b"null".to_vec());
        write_stdin(&mut child, &payload);

        let timeout = Duration::from_secs(def.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let run = match run_bounded(child, OUTPUT_CAP_BYTES, timeout, Some(&ctx.cancel)).await {
            Ok(run) => run,
            Err(err) => return ToolOutput::err(format!("subprocess configuration error: {err}")),
        };

        let (exit_code, note) = match run.outcome {
            Outcome::Exited(code) => (code, String::new()),
            Outcome::TimedOut => (
                None,
                format!(
                    "timed out after {}s; killed the process group. ",
                    timeout.as_secs()
                ),
            ),
            Outcome::Cancelled => (None, "cancelled; killed the process group. ".to_string()),
        };
        let truncated = run.stdout.truncated || run.stderr.truncated;
        let stdout = stream_text(&run.stdout);
        let stderr = stream_text(&run.stderr);
        let failed = !matches!(exit_code, Some(0)) || truncated;
        let text = if failed {
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into());
            format!("{note}exit code {code}\nstdout:\n{stdout}stderr:\n{stderr}")
        } else {
            stdout
        };
        if failed {
            ToolOutput::err(text)
        } else {
            ToolOutput::ok(text)
        }
    }
}

/// 压平单路有界输出：保留头部；超限时在末尾接截断摘要（含杀组说明），
/// 让模型拿到可操作的诊断而不只是残缺输出。
fn stream_text(stream: &BoundedOutput) -> String {
    let Some(note) = stream.truncation_note() else {
        return stream.text.clone();
    };
    let mut text = stream.text.clone();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&note);
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn ctx_in(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    /// 造一个插件：plugin.json + dev.instagent/tools/*.json + 十行 shell 脚本。
    struct Harness {
        dir: TempDir,
    }

    fn harness() -> Harness {
        Harness {
            dir: TempDir::new().unwrap(),
        }
    }

    impl Harness {
        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn plugin(&self, name: &str) -> PathBuf {
            self.plugin_with(name, "{}")
        }

        fn plugin_with(&self, name: &str, manifest_ext: &str) -> PathBuf {
            let root = self.root().join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("plugin.json"),
                format!(
                    r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"{}}}"#,
                    if manifest_ext == "{}" {
                        String::new()
                    } else {
                        format!(",\"extensions\":{{\"{NAMESPACE}\":{manifest_ext}}}")
                    }
                ),
            )
            .unwrap();
            root
        }

        fn tool(&self, plugin_root: &Path, file: &str, json: &str) {
            let dir = plugin_root.join(NAMESPACE).join("tools");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), json).unwrap();
        }

        /// 十行 shell 脚本：读 stdin JSON，按 mode 输出/退出。
        fn script(&self, plugin_root: &Path, rel: &str, body: &str) -> PathBuf {
            let path = plugin_root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    fn plugin_set(plugins: &[&Path]) -> PluginSet {
        let mut set = PluginSet::default();
        for root in plugins {
            let manifest = crate::plugin::manifest::read_manifest(root).unwrap();
            set.plugins.push(Plugin {
                manifest,
                root: root.to_path_buf(),
                source: crate::plugin::PluginSource::Extra,
            });
        }
        set
    }

    fn weather_def(script_rel: &str, timeout: Option<u64>) -> String {
        let timeout = timeout
            .map(|t| format!(",\"timeout_secs\":{t}"))
            .unwrap_or_default();
        format!(
            r#"{{"name":"weather","description":"Get current weather for a city",
            "input_schema":{{"type":"object","properties":{{"city":{{"type":"string"}}}},"required":["city"]}},
            "command":"${{PLUGIN_ROOT}}/{script_rel}"{timeout},"read_only":true}}"#
        )
    }

    #[tokio::test]
    async fn normal_run_returns_stdout_with_namespaced_tool() {
        let h = harness();
        let plugin = h.plugin("weathr");
        // 十行 shell 脚本：从 stdin 的 input JSON 提取 city。
        h.script(
            &plugin,
            "scripts/weather.sh",
            concat!(
                "# command tool fixture: read input JSON from stdin\n",
                "city=$(sed 's/.*\"city\":\"//; s/\".*//')\n",
                "if [ -z \"$city\" ]; then\n",
                "  echo \"no city given\" >&2\n",
                "  exit 2\n",
                "fi\n",
                "echo \"sunny in $city\"\n",
                "# end of tool\n",
            ),
        );
        h.tool(
            &plugin,
            "weather.json",
            &weather_def("scripts/weather.sh", None),
        );

        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, "cmd:weathr");

        let specs = sources[0].list().await;
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "weathr__weather", "工具名 <plugin>__<tool>");
        assert!(specs[0].read_only);
        assert_eq!(specs[0].description, "Get current weather for a city");

        let out = sources[0]
            .call(
                "weathr__weather",
                json!({"city": "Tokyo"}),
                &ctx_in(h.root()),
            )
            .await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.text, "sunny in Tokyo\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_with_streams() {
        let h = harness();
        let plugin = h.plugin("failing");
        h.script(
            &plugin,
            "scripts/fail.sh",
            "echo partial-out\necho boom >&2\nexit 7",
        );
        h.tool(
            &plugin,
            "fail.json",
            &weather_def("scripts/fail.sh", Some(10)).replace("weather", "fail"),
        );

        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let out = sources[0]
            .call("failing__fail", json!({}), &ctx_in(h.root()))
            .await;
        assert!(out.is_error);
        assert!(out.text.contains("exit code 7"), "{}", out.text);
        assert!(out.text.contains("boom"));
        assert!(out.text.contains("partial-out"));
    }

    /// pid 落盘的有界轮询（10ms 一次、最多 0.5s）。只在 call 返回（组已
    /// SIGKILL）之后调用：文件此时不再变化——有则进程必然启动过（第一行
    /// 就是 echo），无则说明它直到超时都没启动。
    async fn wait_pid_file(path: &Path) -> Option<u32> {
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Some(pid) = text.trim().parse::<u32>().ok().filter(|p| *p > 0) {
                    return Some(pid);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    /// 10ms 轮询、最多 5s：pid 不可信号即通过，超时才判失败。
    async fn assert_group_dead(pid: u32) {
        for _ in 0..500 {
            if unsafe { kill(pid as i32, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("tool process {pid} still alive after group kill");
    }

    #[tokio::test]
    async fn timeout_kills_process_group_and_is_error() {
        const TIMEOUT_SECS: u64 = 3;
        let h = harness();
        let plugin = h.plugin("slowpoke");
        let pid_file = h.root().join("tool.pid");
        // 脚本自己落 pid，再挂住不退出；极端负载下进程若直到超时都没
        // 启动，pid 文件永不出现，届时跳过组杀检查（见测试尾部注释）。
        h.script(
            &plugin,
            "scripts/slow.sh",
            &format!("echo $$ > {}\nsleep 120", pid_file.display()),
        );
        h.tool(
            &plugin,
            "slow.json",
            &weather_def("scripts/slow.sh", Some(TIMEOUT_SECS)).replace("weather", "slow"),
        );

        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let ctx = ctx_in(h.root());
        let started = std::time::Instant::now();
        let out = sources[0].call("slowpoke__slow", json!({}), &ctx).await;
        assert!(out.is_error);
        assert!(
            out.text
                .contains(&format!("timed out after {TIMEOUT_SECS}s")),
            "{}",
            out.text
        );
        assert!(started.elapsed() < Duration::from_secs(30));
        // 组杀检查只在观测到 pid 时做：进程没在超时前启动就没有可观测
        // 对象，跳过不影响超时→is_error 的决策断言。
        if let Some(pid) = wait_pid_file(&pid_file).await {
            assert_group_dead(pid).await;
        }
    }

    #[tokio::test]
    async fn cancellation_is_error() {
        let h = harness();
        let plugin = h.plugin("cancelme");
        h.script(&plugin, "scripts/sleep.sh", "sleep 120");
        h.tool(
            &plugin,
            "nap.json",
            &weather_def("scripts/sleep.sh", Some(300)).replace("weather", "nap"),
        );

        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let token = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: h.root().to_path_buf(),
            cancel: token.clone(),
        };
        let call =
            tokio::spawn(async move { sources[0].call("cancelme__nap", json!({}), &ctx).await });
        // sources 被 move 进 task，先取出再等待不行；这里轮询稍候取消。
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
        let out = call.await.expect("call task");
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"));
    }

    #[test]
    fn load_skips_plugins_without_tools_and_invalid_files() {
        let h = harness();
        let empty = h.plugin("empty");
        let broken = h.plugin("broken");
        h.tool(&broken, "not-json.json", "{ not json");
        h.tool(
            &broken,
            "bad-name.json",
            r#"{"name":"a__b","description":"d","input_schema":{},"command":"true"}"#,
        );
        h.tool(
            &broken,
            "no-command.json",
            r#"{"name":"ok","description":"d","input_schema":{},"command":"  "}"#,
        );
        let good = h.plugin("good");
        h.script(&good, "s.sh", "echo ok");
        h.tool(
            &good,
            "t.json",
            r#"{"name":"t","description":"d","input_schema":{"type":"object"},"command":"${PLUGIN_ROOT}/s.sh"}"#,
        );
        h.tool(
            &good,
            "t-dup.json",
            r#"{"name":"t","description":"dup","input_schema":{"type":"object"},"command":"true"}"#,
        );

        let set = plugin_set(&[&empty, &broken, &good]);
        let sources = CommandTools::load(&set).unwrap();
        assert_eq!(sources.len(), 1, "只留下有有效工具的插件");
        assert_eq!(sources[0].id, "cmd:good");
        assert_eq!(sources[0].tools.len(), 1, "非法与重名工具跳过");
    }

    /// 超过 [`MAX_TOOL_DEF_BYTES`] 的工具定义跳过（带来源诊断的 warn），
    /// 不阻塞同插件外其它插件；巨型 JSON 不无界进内存。
    #[test]
    fn oversized_tool_definition_is_skipped_with_source() {
        let h = harness();
        let big = h.plugin("big");
        let huge_desc = "x".repeat(MAX_TOOL_DEF_BYTES as usize + 1);
        h.tool(
            &big,
            "big.json",
            &format!(
                r#"{{"name":"big","description":"{huge_desc}","input_schema":{{}},"command":"true"}}"#
            ),
        );
        let good = h.plugin("good");
        h.script(&good, "s.sh", "echo ok");
        h.tool(
            &good,
            "t.json",
            r#"{"name":"t","description":"d","input_schema":{},"command":"${PLUGIN_ROOT}/s.sh"}"#,
        );
        let set = plugin_set(&[&big, &good]);
        let sources = CommandTools::load(&set).unwrap();
        assert_eq!(sources.len(), 1, "超限定义的插件没有可用工具");
        assert_eq!(sources[0].id, "cmd:good");
    }

    #[tokio::test]
    async fn unknown_tool_names_error() {
        let h = harness();
        let plugin = h.plugin("solo");
        h.script(&plugin, "s.sh", "echo x");
        h.tool(
            &plugin,
            "t.json",
            r#"{"name":"t","description":"d","input_schema":{},"command":"${PLUGIN_ROOT}/s.sh"}"#,
        );
        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let out = sources[0].call("nope", json!({}), &ctx_in(h.root())).await;
        assert!(out.is_error);
        assert!(out.text.contains("unknown tool"));
    }

    /// ADR 0003 D2：command 工具走插件环境 baseline——只见 `PATH` 等基线、
    /// `PLUGIN_ROOT` 与 manifest 声明的变量；未声明的 key（模拟
    /// `OPENAI_API_KEY` 等凭据）读不到。
    #[tokio::test]
    async fn env_baseline_only_declared_vars_visible() {
        let h = harness();
        let plugin = h.plugin_with("envp", r#"{"env":["INSTAGENT_CMD_DECLARED"]}"#);
        h.script(
            &plugin,
            "env.sh",
            "printf 'root=%s\\ndeclared=%s\\nundeclared=%s\\nhas_path=%s\\n' \
             \"$PLUGIN_ROOT\" \"${INSTAGENT_CMD_DECLARED:-absent}\" \
             \"${INSTAGENT_CMD_UNDECLARED:-absent}\" \
             \"$([ -n \"${PATH:-}\" ] && echo yes || echo no)\"",
        );
        h.tool(
            &plugin,
            "env.json",
            &weather_def("env.sh", None).replace("weather", "env"),
        );
        std::env::set_var("INSTAGENT_CMD_DECLARED", "yes");
        std::env::set_var("INSTAGENT_CMD_UNDECLARED", "leak");
        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let out = sources[0]
            .call("envp__env", json!({}), &ctx_in(h.root()))
            .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains(&format!("root={}", plugin.display())),
            "PLUGIN_ROOT 应经环境变量可见：{}",
            out.text
        );
        assert!(out.text.contains("declared=yes"), "{}", out.text);
        assert!(
            out.text.contains("undeclared=absent"),
            "白名单外变量泄漏：{}",
            out.text
        );
        assert!(out.text.contains("has_path=yes"), "{}", out.text);
    }

    /// 插件根路径含空格、引号、换行与命令替换：命令里的 `${PLUGIN_ROOT}`
    /// 经环境变量传递、在引号内展开（ADR 0003 D2 / S11），路径按字面到达
    /// 脚本，`$(…)` 不被执行、命令含义不被改变。
    #[tokio::test]
    async fn hostile_plugin_root_path_passes_literally() {
        let h = harness();
        let injected = h.root().join("injected");
        let evil = format!("plu gin'\"q\"\n$(touch {})", injected.display());
        let plugin = h.root().join(&evil);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"p","version":"1.0.0"}}"#),
        )
        .unwrap();
        h.script(&plugin, "s.sh", "echo literally-ok");
        let dir = plugin.join(NAMESPACE).join("tools");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("t.json"),
            r#"{"name":"t","description":"d","input_schema":{},"command":"\"${PLUGIN_ROOT}\"/s.sh"}"#,
        )
        .unwrap();
        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let out = sources[0].call("p__t", json!({}), &ctx_in(h.root())).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("literally-ok"), "{}", out.text);
        assert!(
            !injected.exists(),
            "路径里的 $(…) 被执行了：命令含义被路径改变"
        );
    }

    /// 输出超过 [`OUTPUT_CAP_BYTES`]：进程组被杀、is_error，stdout 带
    /// 截断摘要（可控 fake：/dev/zero 洪泛，只有杀组后才能收敛）。
    #[tokio::test]
    async fn output_over_cap_kills_group_and_is_error() {
        let h = harness();
        let plugin = h.plugin("flood");
        h.script(
            &plugin,
            "flood.sh",
            "head -c 1200000 /dev/zero | tr '\\0' a",
        );
        h.tool(
            &plugin,
            "flood.json",
            &weather_def("flood.sh", Some(30)).replace("weather", "flood"),
        );
        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let started = std::time::Instant::now();
        let out = sources[0]
            .call("flood__flood", json!({}), &ctx_in(h.root()))
            .await;
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(out.is_error);
        assert!(out.text.contains("output truncated"), "{}", out.text);
        assert!(out.text.contains("process group killed"), "{}", out.text);
    }
}
