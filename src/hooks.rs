//! Hooks（第三版 §2.7）：格式、载荷、决策协议逐字沿用 goose，同一份脚本
//! goose / instagent 两边都能用。`HookContext` 字段名与决策判定（exit 2、
//! stdout `{"decision":"block"}`、exit 0 空/allow、其余按 on_failure）搬运自
//! goose `hooks/mod.rs`（commit 4ad43df，`HookContext` 225~258、
//! `classify_output` 916~966），其余为本仓库重写。
//!
//! 与 goose 的有意差异：环境变量是白名单透传（`PATH` `HOME` `LANG` +
//! manifest `extensions["dev.instagent"].env` 声明的名字）+ `PLUGIN_ROOT`，
//! goose 是全量继承；`on_failure` 对两个可阻止事件（PreToolUse / Stop）都
//! 生效，goose 只认 PreToolUse。载荷字段取 §2.7 的子集（无
//! `matcher_context` / `tool_call_id` / `last_assistant_message`，Stop 的
//! 最后一条助手文本放 `message`）。
//!
//! ADR 0003 钉死的策略：
//! - D2：插件子进程环境 baseline + manifest allowlist（[`apply_plugin_env`]，
//!   command 工具复用，builtin shell 除外保留父环境）；`${PLUGIN_ROOT}` 一律经
//!   `PLUGIN_ROOT` 环境变量传递、由 shell 自行展开，**不**在加载期字符串替换
//!   进 `sh -c` 代码（路径含空格 / 引号 / metacharacter 时不破坏解析、不改变
//!   命令含义）。
//! - D3：`on_failure` 缺省 `Allow`（fail-open），显式 `"block"` 升级 fail-closed；
//!   失败不再静默——spawn 失败 / 超时 / 输出超限 / 无决策全部产出带插件、事件、
//!   命令、原因的 warning。

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use crate::plugin::Plugin;
use crate::plugin::PluginSet;
use crate::plugin::NAMESPACE;
use crate::subprocess::run_bounded;
use crate::subprocess::write_stdin;
use crate::subprocess::Outcome;
use crate::subprocess::ProcessGroupChild;

/// 默认超时 30s（§2.7）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 每路输出收集硬上限：hook 决策载荷很小，64 KiB 足够；超限杀整个进程组，
/// 截断状态经 [`crate::subprocess::BoundedOutput::truncated`] 上报（todo 06 / R3）。
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;
/// Stop 连续阻止上限（goose 默认 8），防死循环。
pub const STOP_BLOCK_LIMIT: u32 = 8;
/// 载荷字段按事件省略，`event` 与 `session_id` 必有。
const EMPTY_REASON_DENY: &str = "denied by plugin hook";

/// v1 六个事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    SessionEnd,
}

impl HookEvent {
    pub fn name(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "SessionStart" => HookEvent::SessionStart,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "PreToolUse" => HookEvent::PreToolUse,
            "PostToolUse" => HookEvent::PostToolUse,
            "Stop" => HookEvent::Stop,
            "SessionEnd" => HookEvent::SessionEnd,
            _ => return None,
        })
    }

    /// 只有 PreToolUse 和 Stop 能阻止（§2.7）。
    pub fn can_block(&self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::Stop)
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// stdin 载荷 JSON（字段名照 goose HookContext 的 v1 子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookContext {
    pub event: HookEvent,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
}

impl HookContext {
    pub fn new(event: HookEvent, session_id: impl Into<String>) -> Self {
        Self {
            event,
            session_id: session_id.into(),
            tool_name: None,
            tool_input: None,
            tool_output: None,
            message: None,
            working_dir: None,
        }
    }

    pub fn with_tool(mut self, name: impl Into<String>, input: Option<Value>) -> Self {
        self.tool_name = Some(name.into());
        self.tool_input = input;
        self
    }

    pub fn with_tool_output(mut self, output: impl Into<String>) -> Self {
        self.tool_output = Some(output.into());
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// matcher 的匹配目标（goose 的 matcher_context）：工具事件是工具名，
    /// UserPromptSubmit / Stop 是 message，其余为空串。
    fn matcher_target(&self) -> &str {
        self.tool_name
            .as_deref()
            .or(self.message.as_deref())
            .unwrap_or("")
    }
}

/// 无决策时怎么办（默认 allow）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    #[default]
    Allow,
    Block,
}

/// hooks.json 里一条 hook 定义（v1 只有 type = "command"，缺省同 goose 视为
/// command；其余 type 加载时跳过）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDef {
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default, rename = "on_failure")]
    pub on_failure: Option<OnFailure>,
}

/// 一个 matcher 组：`{"matcher": "shell|everything__.*", "hooks": [...]}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMatcherGroup {
    /// 正则匹配工具名；省略则每次都跑。
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookDef>,
}

/// `dev.instagent/hooks.json` 顶层：`{"hooks": {"PreToolUse": [...]}}`。
/// 事件名用字符串键：未知事件按规范忽略不报错（同 goose）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<HookMatcherGroup>>,
}

/// 单个 hook 及其来源插件；command 原样保留 `${PLUGIN_ROOT}`（运行时经
/// `PLUGIN_ROOT` 环境变量由 shell 展开，ADR 0003 D2），matcher 已预编译。
#[derive(Debug, Clone)]
pub struct RegisteredHook {
    pub plugin: String,
    pub plugin_root: PathBuf,
    pub event: HookEvent,
    pub matcher: Option<Regex>,
    pub group: HookMatcherGroup,
}

/// 决策：退出码 2 → Block(stderr)；stdout `{"decision":"block"}` → Block；
/// 退出 0 且空/allow → Allow；其余按 on_failure（Allow 时返回 None）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Block(String),
    None,
}

/// 全部启用插件的 hooks 汇总。
#[derive(Debug, Default)]
pub struct Hooks {
    pub entries: Vec<RegisteredHook>,
    /// 插件 → manifest `extensions[dev.instagent].env` 声明的环境变量名。
    plugin_env: BTreeMap<String, Vec<String>>,
}

impl Hooks {
    /// 加载：`dev.instagent/hooks.json`，没有则回退草案位置
    /// `hooks/hooks.json`；matcher 预编译。命令里的 `${PLUGIN_ROOT}` 不在加载期
    /// 展开，运行时经 `PLUGIN_ROOT` 环境变量由 shell 展开（ADR 0003 D2）。
    /// 未知事件、非 command 类型、非法正则的 rule 跳过（warn）。
    pub fn load(plugins: &PluginSet) -> crate::Result<Hooks> {
        let mut out = Hooks::default();
        for plugin in plugins.iter() {
            let Some(path) = hooks_file_path(plugin) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let parsed: HooksFile = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            let declared = declared_env(plugin);
            if !declared.is_empty() {
                out.plugin_env
                    .insert(plugin.manifest.name.clone(), declared);
            }
            for (event_name, groups) in parsed.hooks {
                let Some(event) = HookEvent::from_name(&event_name) else {
                    tracing::warn!("{}: 忽略未知 hook 事件 `{event_name}`", path.display());
                    continue;
                };
                for group in groups {
                    let HookMatcherGroup {
                        matcher: matcher_src,
                        hooks: raw_hooks,
                    } = group;
                    let matcher = match matcher_src.as_deref().filter(|s| !s.is_empty()) {
                        Some(pattern) => match Regex::new(pattern) {
                            Ok(re) => Some(re),
                            Err(err) => {
                                tracing::warn!(
                                    "{}: matcher `{pattern}` 不是合法正则（{err}），跳过该 rule",
                                    path.display()
                                );
                                continue;
                            }
                        },
                        None => None,
                    };
                    let hooks: Vec<HookDef> = raw_hooks
                        .into_iter()
                        .filter(|hook| {
                            let keep = hook.r#type.as_deref().is_none_or(|t| t == "command");
                            if !keep {
                                tracing::warn!(
                                    "{}: 忽略不支持的 hook 类型 `{}`",
                                    path.display(),
                                    hook.r#type.as_deref().unwrap_or_default()
                                );
                            }
                            keep
                        })
                        .collect();
                    if hooks.is_empty() {
                        continue;
                    }
                    out.entries.push(RegisteredHook {
                        plugin: plugin.manifest.name.clone(),
                        plugin_root: plugin.root.clone(),
                        event,
                        matcher,
                        group: HookMatcherGroup {
                            matcher: matcher_src,
                            hooks,
                        },
                    });
                }
            }
        }
        Ok(out)
    }

    /// 跑该事件全部匹配 hook（按插件名、加载顺序串行）：首个可阻止事件的
    /// Block 短路返回 `Block("[plugin] reason")`；其余继续。不可阻止事件的
    /// Block 与 None 都视为放行。
    pub async fn run(&self, ctx: &HookContext) -> HookDecision {
        let payload = match serde_json::to_string(ctx) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!(
                    "hook 载荷序列化失败（事件 {}：{err}），按放行处理",
                    ctx.event
                );
                return HookDecision::Allow;
            }
        };
        let target = ctx.matcher_target();
        for entry in &self.entries {
            if entry.event != ctx.event {
                continue;
            }
            if let Some(matcher) = &entry.matcher {
                if !matcher.is_match(target) {
                    continue;
                }
            }
            let declared = self
                .plugin_env
                .get(&entry.plugin)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for hook in &entry.group.hooks {
                let decision = run_hook(
                    hook,
                    &entry.plugin,
                    entry.event,
                    &entry.plugin_root,
                    declared,
                    &payload,
                    ctx.working_dir.as_deref(),
                )
                .await;
                if let HookDecision::Block(reason) = decision {
                    if !entry.event.can_block() {
                        tracing::debug!(
                            "事件 {} 不可阻止，忽略 `{}` 的 block（{reason}）",
                            entry.event,
                            entry.plugin
                        );
                        continue;
                    }
                    tracing::info!("hook 阻止 {}：[{}] {reason}", entry.event, entry.plugin);
                    return HookDecision::Block(format!("[{}] {reason}", entry.plugin));
                }
            }
        }
        HookDecision::Allow
    }
}

/// `dev.instagent/hooks.json` 优先，回退 goose 草案位置 `hooks/hooks.json`。
fn hooks_file_path(plugin: &Plugin) -> Option<PathBuf> {
    let namespaced = plugin.root.join(NAMESPACE).join("hooks.json");
    if namespaced.is_file() {
        return Some(namespaced);
    }
    let draft = plugin.root.join("hooks").join("hooks.json");
    draft.is_file().then_some(draft)
}

/// manifest `extensions["dev.instagent"].env`：字符串数组，其余形状忽略。
/// hooks 与 command 工具加载期共用（ADR 0003 D2 allowlist）。
pub fn declared_env(plugin: &Plugin) -> Vec<String> {
    plugin
        .manifest
        .extensions
        .get(NAMESPACE)
        .and_then(|ns| ns.get("env"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// ADR 0003 D2 插件子进程环境 baseline：`env_clear` 后只注入 `PATH` `HOME`
/// `LANG`（存在才带）、`PLUGIN_ROOT` 与 manifest 声明的变量名。hooks 与
/// command 工具共用（MCP stdio 由 todo 10 接入）；默认不泄露 provider
/// credentials / session secrets。builtin shell 是唯一例外，保留父环境。
pub fn apply_plugin_env(cmd: &mut Command, plugin_root: &Path, declared: &[String]) {
    cmd.env_clear();
    for key in ["PATH", "HOME", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("PLUGIN_ROOT", plugin_root);
    for name in declared {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
}

/// 决策判定（搬运 goose `classify_output`，hooks/mod.rs:916~966）：
/// 退出 2 → Block(stderr)；stdout JSON `decision:"block"` → Block（不看退出码）；
/// 退出 0 且空或 `decision:"allow"` → Allow；其余"没有决策"按 on_failure
/// （Allow → None，Block → Block(框架生成的失败理由)）。
///
/// 第二个返回值是"没有决策"时的有界失败诊断（ADR 0003 D3 可见性）；
/// 产出合法 allow / block 决策时为 `None`。
pub fn parse_decision(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    on_failure: OnFailure,
) -> (HookDecision, Option<String>) {
    let non_empty = |s: &str| {
        if s.is_empty() {
            EMPTY_REASON_DENY.to_string()
        } else {
            s.to_string()
        }
    };

    if exit_code == Some(2) {
        return (HookDecision::Block(non_empty(stderr.trim())), None);
    }

    #[derive(Deserialize)]
    struct Resp {
        decision: Option<String>,
        reason: Option<String>,
    }

    let trimmed = stdout.trim();
    let resp = trimmed
        .starts_with('{')
        .then(|| serde_json::from_str::<Resp>(trimmed).ok())
        .flatten();

    if let Some(resp) = &resp {
        if resp.decision.as_deref() == Some("block") {
            let reason = resp.reason.clone().unwrap_or_default();
            return (HookDecision::Block(non_empty(reason.trim())), None);
        }
    }
    if exit_code == Some(0)
        && (trimmed.is_empty()
            || resp.as_ref().and_then(|r| r.decision.as_deref()) == Some("allow"))
    {
        return (HookDecision::Allow, None);
    }

    let diagnosis = match exit_code {
        Some(0) => "exited 0 without an allow or block decision on stdout".to_string(),
        Some(code) => format!("exited with status {code} and no usable decision"),
        None => "was terminated by a signal".to_string(),
    };
    let decision = match on_failure {
        OnFailure::Allow => HookDecision::None,
        OnFailure::Block => HookDecision::Block(format!("the hook {diagnosis}")),
    };
    (decision, Some(diagnosis))
}

/// hook 失败收敛（ADR 0003 D3）：产出带插件 / 事件 / 命令 / 原因的 warning，
/// 再按 on_failure 决策——Allow（缺省）放行，显式 `"block"` 阻断。
fn fail_with_warning(
    plugin: &str,
    event: HookEvent,
    command: &str,
    on_failure: OnFailure,
    reason: &str,
) -> HookDecision {
    let policy = match on_failure {
        OnFailure::Allow => "fail-open（on_failure 缺省 allow）",
        OnFailure::Block => "fail-closed（on_failure: block）",
    };
    tracing::warn!("[{plugin}] {event} hook `{command}` 失败：{reason}；策略 {policy}");
    match on_failure {
        OnFailure::Allow => HookDecision::None,
        OnFailure::Block => HookDecision::Block(format!("the hook {reason}")),
    }
}

/// hook 子进程：`sh -c` + baseline 白名单环境（[`apply_plugin_env`]）；
/// working_dir 存在则作为 cwd。`${PLUGIN_ROOT}` 不替换进命令字符串，
/// 由 shell 经环境变量展开（ADR 0003 D2）。
fn hook_command(
    command: &str,
    plugin_root: &Path,
    declared: &[String],
    working_dir: Option<&Path>,
) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    apply_plugin_env(&mut cmd, plugin_root, declared);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    cmd
}

/// 跑一条 command hook：载荷写 stdin，输出走 [`run_bounded`] 有界收集
/// （每路 [`OUTPUT_CAP_BYTES`]），超时 / 超限 / 取消时 drop
/// [`ProcessGroupChild`] 杀整个进程组（`03`）。失败全部经
/// [`fail_with_warning`] 可见，并按 on_failure 收敛（ADR 0003 D3）。
async fn run_hook(
    hook: &HookDef,
    plugin: &str,
    event: HookEvent,
    plugin_root: &Path,
    declared_env: &[String],
    payload: &str,
    working_dir: Option<&Path>,
) -> HookDecision {
    let on_failure = hook.on_failure.unwrap_or_default();
    let timeout = Duration::from_secs(hook.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut cmd = hook_command(&hook.command, plugin_root, declared_env, working_dir);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match ProcessGroupChild::spawn(&mut cmd) {
        Ok(child) => child,
        Err(err) => {
            return fail_with_warning(
                plugin,
                event,
                &hook.command,
                on_failure,
                &format!("failed to spawn: {err}"),
            );
        }
    };

    // 载荷后台写 stdin（防大输入死锁；脚本不读时 BrokenPipe 忽略）。
    write_stdin(&mut child, payload.as_bytes());
    let run = match run_bounded(child, OUTPUT_CAP_BYTES, timeout, None).await {
        Ok(run) => run,
        Err(err) => {
            return fail_with_warning(
                plugin,
                event,
                &hook.command,
                on_failure,
                &format!("subprocess configuration error: {err}"),
            );
        }
    };
    if run.outcome == Outcome::TimedOut {
        return fail_with_warning(
            plugin,
            event,
            &hook.command,
            on_failure,
            &format!(
                "timed out after {}s; killed the process group",
                timeout.as_secs()
            ),
        );
    }
    if run.stdout.truncated || run.stderr.truncated {
        return fail_with_warning(
            plugin,
            event,
            &hook.command,
            on_failure,
            &format!("output exceeded {OUTPUT_CAP_BYTES} bytes; killed the process group"),
        );
    }
    let exit_code = match run.outcome {
        Outcome::Exited(code) => code,
        _ => None,
    };
    let (decision, diagnosis) =
        parse_decision(&run.stdout.text, &run.stderr.text, exit_code, on_failure);
    if let Some(diagnosis) = diagnosis {
        fail_with_warning(plugin, event, &hook.command, on_failure, &diagnosis);
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PLUGIN_SCHEMA_URL;
    use crate::plugin::PluginSource;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// 造插件：plugin.json + hooks.json（规范位置或草案位置）+ 脚本。
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

        /// hooks_json 写到 `dev.instagent/hooks.json`（force_draft 时写到
        /// `hooks/hooks.json` 草案位置）。
        fn hooks_file(&self, plugin_root: &Path, hooks_json: &str, draft: bool) {
            let dir = if draft {
                plugin_root.join("hooks")
            } else {
                plugin_root.join(NAMESPACE)
            };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hooks.json"), hooks_json).unwrap();
        }

        fn script(&self, plugin_root: &Path, rel: &str, body: &str) -> PathBuf {
            let path = plugin_root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    fn plugin_set(roots: &[&Path]) -> PluginSet {
        let mut set = PluginSet::default();
        for root in roots {
            let manifest = crate::plugin::manifest::read_manifest(root).unwrap();
            set.plugins.push(Plugin {
                manifest,
                root: root.to_path_buf(),
                source: PluginSource::Extra,
            });
        }
        set
    }

    /// 单插件、单事件、命令数组 JSON（`[{...}]`）拼 hooks.json。
    fn hooks_json(event: &str, groups: &str) -> String {
        format!(r#"{{"hooks":{{"{event}":{groups}}}}}"#)
    }

    fn tool_ctx(event: HookEvent) -> HookContext {
        HookContext::new(event, "s1")
            .with_tool("shell", Some(json!({"command": "echo hi"})))
            .with_working_dir("/tmp")
    }

    // ---- 决策路径（R5：每种一条脚本） ----

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(&plugin, "s.sh", "echo nope >&2\nexit 2");
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                r#"[{"hooks":[{"command":"${PLUGIN_ROOT}/s.sh"}]}]"#,
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
        assert_eq!(
            decision,
            HookDecision::Block("[p] nope".into()),
            "退出码 2 → 阻止，理由取 stderr"
        );
    }

    #[tokio::test]
    async fn stdout_block_decides_regardless_of_exit_code() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(
            &plugin,
            "s.sh",
            r#"printf '%s' '{"decision":"block","reason":"policy says no"}'"#,
        );
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                r#"[{"hooks":[{"command":"${PLUGIN_ROOT}/s.sh"}]}]"#,
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
        assert_eq!(
            decision,
            HookDecision::Block("[p] policy says no".into()),
            "stdout block 不看退出码"
        );
    }

    #[tokio::test]
    async fn exit_zero_empty_and_allow_pass() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(&plugin, "empty.sh", "true");
        h.script(&plugin, "allow.sh", r#"printf '%s' '{"decision":"allow"}'"#);
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                &json!([{ "hooks": [
                    {"command": "${PLUGIN_ROOT}/empty.sh"},
                    {"command": "${PLUGIN_ROOT}/allow.sh"},
                ] }])
                .to_string(),
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        assert_eq!(
            hooks.run(&tool_ctx(HookEvent::PreToolUse)).await,
            HookDecision::Allow
        );
    }

    #[tokio::test]
    async fn garbage_stdout_follows_on_failure() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(&plugin, "garbage.sh", "echo not a decision");
        for (on_failure, is_allow) in [(false, true), (true, false)] {
            let action = if on_failure {
                json!({"command": "${PLUGIN_ROOT}/garbage.sh", "on_failure": "block"})
            } else {
                json!({"command": "${PLUGIN_ROOT}/garbage.sh"})
            };
            h.hooks_file(
                &plugin,
                &hooks_json("PreToolUse", &json!([{ "hooks": [action] }]).to_string()),
                false,
            );
            let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
            let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
            if is_allow {
                assert_eq!(decision, HookDecision::Allow, "乱输出默认放行");
            } else {
                // on_failure=block 时框架理由不含脚本输出，只给有界说明。
                match decision {
                    HookDecision::Block(reason) => assert!(
                        reason.starts_with("[p] the hook") && !reason.contains("not a decision"),
                        "{reason}"
                    ),
                    other => panic!("expected block, got {other:?}"),
                }
            }
        }
    }

    /// pid 落盘的有界轮询（10ms 一次、最多 0.5s）。只在 run 返回（组已
    /// SIGKILL）之后调用：文件此时不再变化——有则进程必然启动过（第一行
    /// 就是 echo），无则说明它直到超时都没启动。
    async fn wait_pid_file(path: &Path) -> Option<i32> {
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Some(pid) = text.trim().parse::<i32>().ok().filter(|p| *p > 0) {
                    return Some(pid);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    /// 10ms 轮询、最多 5s：pid 不可信号即通过，超时才判失败。
    async fn assert_group_dead(pid: i32) {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        for _ in 0..500 {
            if unsafe { kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("hook process {pid} still alive after group kill");
    }

    #[tokio::test]
    async fn timeout_is_no_decision_and_kills_group() {
        let h = harness();
        let plugin = h.plugin("p");
        let pid_file = h.root().join("hook.pid");
        h.script(
            &plugin,
            "slow.sh",
            &format!("echo $$ > {}\nsleep 120", pid_file.display()),
        );
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                &json!([{"hooks":[{"command":"${PLUGIN_ROOT}/slow.sh","timeout":3,"on_failure":"block"}]}])
                    .to_string(),
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let started = std::time::Instant::now();
        let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
        assert!(started.elapsed() < Duration::from_secs(30));
        match decision {
            HookDecision::Block(reason) => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("expected timeout block, got {other:?}"),
        }
        // 组杀检查只在观测到 pid 时做：极端负载下进程若直到超时都没启动，
        // 没有可观测对象，跳过即可；决策（超时→on_failure block）断言不变。
        if let Some(pid) = wait_pid_file(&pid_file).await {
            assert_group_dead(pid).await;
        }
    }

    // ---- 载荷与事件规则 ----

    #[tokio::test]
    async fn payload_fields_match_goose_context() {
        let h = harness();
        let payload_file = h.root().join("payload.json");
        let plugin = h.plugin("p");
        h.script(
            &plugin,
            "cat.sh",
            &format!("cat > {}", payload_file.display()),
        );
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PostToolUse",
                r#"[{"hooks":[{"command":"${PLUGIN_ROOT}/cat.sh"}]}]"#,
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let ctx = HookContext::new(HookEvent::PostToolUse, "sess-9")
            .with_tool("shell", Some(json!({"command": "ls"})))
            .with_tool_output("out")
            .with_working_dir(h.root());
        assert_eq!(hooks.run(&ctx).await, HookDecision::Allow);
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(payload_file).unwrap()).unwrap();
        assert_eq!(payload["event"], "PostToolUse");
        assert_eq!(payload["session_id"], "sess-9");
        assert_eq!(payload["tool_name"], "shell");
        assert_eq!(payload["tool_input"]["command"], "ls");
        assert_eq!(payload["tool_output"], "out");
        assert_eq!(payload["working_dir"], h.root().display().to_string());
        assert!(payload.get("message").is_none(), "按事件省略空字段");
    }

    #[tokio::test]
    async fn only_pre_tool_use_and_stop_can_block() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(&plugin, "block.sh", "echo never >&2\nexit 2");
        h.hooks_file(
            &plugin,
            &json!({"hooks": {
                "UserPromptSubmit": [{"hooks": [{"command": "${PLUGIN_ROOT}/block.sh"}]}],
                "PostToolUse": [{"hooks": [{"command": "${PLUGIN_ROOT}/block.sh"}]}],
                "SessionStart": [{"hooks": [{"command": "${PLUGIN_ROOT}/block.sh"}]}],
            }})
            .to_string(),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        for event in [
            HookEvent::UserPromptSubmit,
            HookEvent::PostToolUse,
            HookEvent::SessionStart,
        ] {
            let ctx = HookContext::new(event, "s1").with_message("m");
            assert_eq!(
                hooks.run(&ctx).await,
                HookDecision::Allow,
                "{event} 的 Block 必须被忽略"
            );
        }
    }

    #[tokio::test]
    async fn matcher_is_regex_on_tool_name_omitted_always_runs() {
        let h = harness();
        let hit = h.root().join("hit");
        let plugin = h.plugin("p");
        h.script(&plugin, "touch.sh", &format!("touch {}", hit.display()));
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                r#"[{"matcher": "^sh(l)?$","hooks":[{"command":"${PLUGIN_ROOT}/touch.sh"}]}]"#,
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();

        let miss = HookContext::new(HookEvent::PreToolUse, "s1").with_tool("text_editor", None);
        assert_eq!(hooks.run(&miss).await, HookDecision::Allow);
        assert!(!hit.exists(), "正则不匹配工具名则不跑");

        let hit_ctx = HookContext::new(HookEvent::PreToolUse, "s1").with_tool("sh", None);
        assert_eq!(hooks.run(&hit_ctx).await, HookDecision::Allow);
        assert!(hit.exists(), "^sh(l)?$ 匹配 \"sh\"");
    }

    // ---- 加载与展开 ----

    #[test]
    fn namespace_path_wins_draft_path_is_fallback() {
        let h = harness();
        let plugin = h.plugin("p");
        h.script(&plugin, "s.sh", "true");
        h.hooks_file(
            &plugin,
            &hooks_json("Stop", r#"[{"hooks":[{"command":"nope"}]}]"#),
            false,
        );
        h.hooks_file(
            &plugin,
            &hooks_json("PreToolUse", r#"[{"hooks":[{"command":"nope"}]}]"#),
            true,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        assert_eq!(hooks.entries.len(), 1);
        assert_eq!(hooks.entries[0].event, HookEvent::Stop, "规范位置优先");

        // 只有草案位置时回退读取。
        std::fs::remove_file(plugin.join(NAMESPACE).join("hooks.json")).unwrap();
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        assert_eq!(hooks.entries.len(), 1);
        assert_eq!(hooks.entries[0].event, HookEvent::PreToolUse);
    }

    #[test]
    fn unknown_events_and_invalid_matchers_are_skipped() {
        let h = harness();
        let plugin = h.plugin("p");
        h.hooks_file(
            &plugin,
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"command":"true"}]},
                 {"matcher":"ok","hooks":[{"type":"prompt","command":"true"}]}],
                "BeforeReadFile":[{"hooks":[{"command":"true"}]}]}}"#,
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        assert!(
            hooks.entries.is_empty(),
            "非法正则 rule、非 command 类型、未知事件全部跳过"
        );
    }

    #[tokio::test]
    async fn plugin_root_expands_and_env_whitelist_passes_declared_only() {
        let h = harness();
        let out = h.root().join("env.out");
        let plugin = h.plugin_with("p", r#"{"env":["INSTAGENT_HOOK_DECLARED"]}"#);
        // ${PLUGIN_ROOT} 展开：脚本放插件根下，命令带占位符。
        h.script(
            &plugin,
            "env.sh",
            &format!(
                "printf 'root=%s\\ndeclared=%s\\nundeclared=%s\\nhas_path=%s\\n' \
                 \"$PLUGIN_ROOT\" \"${{INSTAGENT_HOOK_DECLARED:-absent}}\" \
                 \"${{INSTAGENT_HOOK_UNDECLARED:-absent}}\" \"$([ -n \"${{PATH:-}}\" ] && echo yes || echo no)\" \
                 > {}",
                out.display()
            ),
        );
        h.hooks_file(
            &plugin,
            &hooks_json(
                "SessionStart",
                r#"[{"hooks":[{"command":"${PLUGIN_ROOT}/env.sh"}]}]"#,
            ),
            false,
        );
        std::env::set_var("INSTAGENT_HOOK_DECLARED", "yes");
        std::env::set_var("INSTAGENT_HOOK_UNDECLARED", "leak");
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let decision = hooks
            .run(&HookContext::new(HookEvent::SessionStart, "s1"))
            .await;
        assert_eq!(decision, HookDecision::Allow);
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(
            text.contains(&format!("root={}", plugin.display())),
            "${{PLUGIN_ROOT}} 未展开：{text}"
        );
        assert!(text.contains("declared=yes"), "{text}");
        assert!(
            text.contains("undeclared=absent"),
            "白名单外变量泄漏：{text}"
        );
        assert!(text.contains("has_path=yes"), "{text}");
    }

    // ---- parse_decision 单元路径（搬运 goose classify_output 的表） ----

    fn pd(code: Option<i32>, stdout: &str, stderr: &str) -> HookDecision {
        parse_decision(stdout, stderr, code, OnFailure::Allow).0
    }

    #[test]
    fn parse_decision_matrix() {
        use OnFailure::Block;
        assert_eq!(
            pd(Some(2), "", "denied"),
            HookDecision::Block("denied".into())
        );
        assert_eq!(
            pd(Some(2), "", ""),
            HookDecision::Block(EMPTY_REASON_DENY.into())
        );
        assert_eq!(
            pd(Some(1), r#"{"decision":"block","reason":"no"}"#, ""),
            HookDecision::Block("no".into()),
            "block 不看退出码"
        );
        assert_eq!(pd(Some(0), "", ""), HookDecision::Allow);
        assert_eq!(pd(Some(0), "  \n", ""), HookDecision::Allow);
        assert_eq!(
            pd(Some(0), r#"{"decision":"allow"}"#, ""),
            HookDecision::Allow
        );
        // 其余一律"没有决策"。
        assert_eq!(pd(Some(1), "", "boom"), HookDecision::None);
        assert_eq!(
            pd(Some(0), r#"{"decision":"maybe"}"#, ""),
            HookDecision::None
        );
        assert_eq!(pd(Some(0), "garbage", ""), HookDecision::None);
        assert_eq!(pd(None, "", ""), HookDecision::None);
        // on_failure=block 把"没有决策"翻成 Block，且理由是框架生成的。
        let (blocked, diagnosis) = parse_decision("garbage", "", Some(0), Block);
        match blocked {
            HookDecision::Block(reason) => assert!(reason.contains("exited 0"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            diagnosis.as_deref(),
            Some("exited 0 without an allow or block decision on stdout"),
            "失败诊断供 warning 使用（ADR 0003 D3）"
        );
        match parse_decision("", "", Some(7), Block).0 {
            HookDecision::Block(reason) => assert!(reason.contains("status 7"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(parse_decision("", "", None, Block).1.is_some());
    }

    /// 合法 allow / block 决策不产出失败诊断；只有"没有决策"才有。
    #[test]
    fn parse_decision_diagnosis_only_on_failure_paths() {
        assert_eq!(
            parse_decision("", "denied", Some(2), OnFailure::Allow).1,
            None
        );
        assert_eq!(
            parse_decision(
                r#"{"decision":"block","reason":"no"}"#,
                "",
                Some(0),
                OnFailure::Allow
            )
            .1,
            None
        );
        assert_eq!(parse_decision("", "", Some(0), OnFailure::Allow).1, None);
        assert_eq!(
            parse_decision(r#"{"decision":"allow"}"#, "", Some(0), OnFailure::Allow).1,
            None
        );
        assert!(parse_decision("garbage", "", Some(1), OnFailure::Allow)
            .1
            .is_some());
    }

    /// 插件根路径含空格、引号、换行与命令替换：`${PLUGIN_ROOT}` 经环境变量
    /// 传递、由 shell 在引号内展开（ADR 0003 D2 / S11）——路径按字面到达
    /// 脚本，不被重新解析，`$(...)` 不会执行、命令含义不被改变。
    #[tokio::test]
    async fn hostile_plugin_root_path_passes_literally() {
        let h = harness();
        let injected = h.root().join("injected");
        // 旧字符串替换方案会把这段目录名拼进 sh -c 代码 → 解析被引号破坏、
        // $(touch …) 被执行。现在只进环境变量，展开结果不再被解析。
        let evil = format!("plu gin'\"q\"\n$(touch {})", injected.display());
        let plugin = h.root().join(&evil);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"p","version":"1.0.0"}}"#),
        )
        .unwrap();
        let ran = h.root().join("ran");
        h.script(&plugin, "s.sh", &format!("touch {}", ran.display()));
        h.hooks_file(
            &plugin,
            &hooks_json(
                "PreToolUse",
                r#"[{"hooks":[{"command":"\"${PLUGIN_ROOT}\"/s.sh"}]}]"#,
            ),
            false,
        );
        let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
        let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
        assert_eq!(decision, HookDecision::Allow);
        assert!(ran.exists(), "带引号展开的脚本应按字面路径执行");
        assert!(
            !injected.exists(),
            "路径里的 $(…) 被执行了：命令含义被路径改变"
        );
    }

    /// 输出超过 [`OUTPUT_CAP_BYTES`]：进程组被杀、按 on_failure 收敛
    /// （缺省放行、显式 block 阻断），理由说明超限（T2 可控 fake：/dev/zero 洪泛）。
    #[tokio::test]
    async fn output_over_cap_kills_group_and_follows_on_failure() {
        let h = harness();
        let plugin = h.plugin("p");
        // 200KB > 64KiB 上限；/dev/zero 洪泛只有在杀组后才能收敛。
        h.script(&plugin, "flood.sh", "head -c 200000 /dev/zero | tr '\\0' a");
        for (block, expect_block) in [(false, false), (true, true)] {
            let hook = if block {
                json!({"command": "${PLUGIN_ROOT}/flood.sh", "on_failure": "block"})
            } else {
                json!({"command": "${PLUGIN_ROOT}/flood.sh"})
            };
            h.hooks_file(
                &plugin,
                &hooks_json("PreToolUse", &json!([{ "hooks": [hook] }]).to_string()),
                false,
            );
            let hooks = Hooks::load(&plugin_set(&[&plugin])).unwrap();
            let started = std::time::Instant::now();
            let decision = hooks.run(&tool_ctx(HookEvent::PreToolUse)).await;
            assert!(started.elapsed() < Duration::from_secs(30));
            if expect_block {
                match decision {
                    HookDecision::Block(reason) => {
                        assert!(reason.contains("exceeded"), "{reason}")
                    }
                    other => panic!("expected block, got {other:?}"),
                }
            } else {
                assert_eq!(decision, HookDecision::Allow, "超限默认 fail-open 放行");
            }
        }
    }
}
