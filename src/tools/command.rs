//! `CommandTools`：`dev.instagent/tools/*.json`，脚本即工具（第三版 §2.9）。
//!
//! 每个启用插件一个实例（id = `cmd:<plugin>`），工具名 `<plugin>__<tool>`
//! （前缀在 [`ToolSource::list`] 里拼好，Registry 只做模型可见名映射）。
//! 执行：`${PLUGIN_ROOT}` 展开后用 `sh -c` 跑（与 hooks 同约定），input JSON
//! 写 stdin，stdout 作为工具结果，退出码非 0 / 超时 / 取消即 `is_error`；
//! 子进程走 `03` 的进程组（[`ProcessGroupChild`]，超时 kill 整组）。
//! 解析失败或非法的 tool JSON 跳过不报错（warn 日志）。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::plugin::Plugin;
use crate::plugin::PluginSet;
use crate::plugin::NAMESPACE;
use crate::subprocess::ProcessGroupChild;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;
use crate::tools::NAME_SEP;

/// `timeout_secs` 缺省值。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 进程退出后给输出管道最多 500ms 收尾（同 builtin shell）。
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// 单个 command tool 定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// 可执行命令；`${PLUGIN_ROOT}` 在调用前展开。
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
    /// 插件根目录；`${PLUGIN_ROOT}` 的展开值（相对命令的基准目录）。
    pub plugin_root: PathBuf,
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
        tools,
    }))
}

fn parse_tool_file(path: &Path) -> crate::Result<CommandToolDef> {
    let text = std::fs::read_to_string(path)?;
    let def: CommandToolDef = serde_json::from_str(&text)?;
    if def.name.is_empty() || def.name.contains(NAME_SEP) || def.name.contains(':') {
        anyhow::bail!("invalid tool name `{}`", def.name);
    }
    if def.command.trim().is_empty() {
        anyhow::bail!("tool `{}` has an empty command", def.name);
    }
    Ok(def)
}

/// `${PLUGIN_ROOT}` → 插件根绝对路径（单次、非递归，与 `06` 展开约定一致）。
fn expand_plugin_root(command: &str, plugin_root: &Path) -> String {
    command.replace("${PLUGIN_ROOT}", &plugin_root.display().to_string())
}

async fn drain(mut handle: JoinHandle<String>) -> String {
    match tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut handle).await {
        Ok(Ok(text)) => text,
        _ => {
            handle.abort();
            String::new()
        }
    }
}

enum Outcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
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
        let command = expand_plugin_root(&def.command, &self.plugin_root);

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        cmd.current_dir(&ctx.cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = match ProcessGroupChild::spawn(&mut cmd) {
            Ok(child) => child,
            Err(err) => {
                return ToolOutput::err(format!("Failed to spawn `{command}`: {err}"));
            }
        };

        // input JSON 写 stdin（后台写，防大输入死锁；脚本不读 stdin 时 BrokenPipe 忽略）。
        let payload = serde_json::to_vec(&input).unwrap_or_else(|_| b"null".to_vec());
        let write_task = child.child_mut().stdin.take().map(|mut stdin| {
            tokio::spawn(async move {
                let _ = stdin.write_all(&payload).await;
                let _ = stdin.shutdown().await;
            })
        });
        let _ = write_task; // 句柄随 child 结束自然完成，无需 join

        let stdout_pipe = child.child_mut().stdout.take().expect("stdout piped");
        let stderr_pipe = child.child_mut().stderr.take().expect("stderr piped");
        let stdout_task = tokio::spawn(read_all(stdout_pipe));
        let stderr_task = tokio::spawn(read_all(stderr_pipe));

        let timeout = Duration::from_secs(def.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
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

        let stdout = drain(stdout_task).await;
        let stderr = drain(stderr_task).await;

        let (exit_code, note) = match outcome {
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
        let failed = !matches!(exit_code, Some(0));
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

async fn read_all<R>(mut reader: R) -> String
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buf = Vec::new();
    if reader.read_to_end(&mut buf).await.is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
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
            let root = self.root().join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("plugin.json"),
                format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"{name}","version":"1.0.0"}}"#),
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
            let validated = crate::plugin::manifest::read_manifest(root).unwrap();
            set.plugins.push(Plugin {
                manifest: validated.manifest,
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

    #[tokio::test]
    async fn timeout_kills_process_group_and_is_error() {
        let h = harness();
        let plugin = h.plugin("slowpoke");
        let pid_file = h.root().join("tool.pid");
        // 脚本自己落 pid（3s 超时的 kill 必然晚于这一行），再挂住不退出。
        h.script(
            &plugin,
            "scripts/slow.sh",
            &format!("echo $$ > {}\nsleep 120", pid_file.display()),
        );
        h.tool(
            &plugin,
            "slow.json",
            &weather_def("scripts/slow.sh", Some(3)).replace("weather", "slow"),
        );

        let set = plugin_set(&[&plugin]);
        let sources = CommandTools::load(&set).unwrap();
        let ctx = ctx_in(h.root());
        let call =
            tokio::spawn(async move { sources[0].call("slowpoke__slow", json!({}), &ctx).await });
        // 超时前确认脚本已启动（否则 pid 断言没有观测对象）。
        for _ in 0..80 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let started = std::time::Instant::now();
        let out = call.await.expect("call task");
        assert!(out.is_error);
        assert!(out.text.contains("timed out after 3s"), "{}", out.text);
        assert!(started.elapsed() < Duration::from_secs(10));

        // 整组被杀：脚本 pid 不再可信号。
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        for _ in 0..100 {
            if unsafe { kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("tool process still alive after group kill");

        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
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
        assert_eq!(
            expand_plugin_root("${PLUGIN_ROOT}/x.sh", Path::new("/p")),
            "/p/x.sh"
        );
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
}
