//! Hooks（第三版 §2.7）：格式、载荷、决策协议逐字沿用 goose，同一份脚本
//! goose / instagent 两边都能用。实现参考 goose `hooks/mod.rs` 的
//! `HookContext`（225~258）与决策判定，其余重写（搬运注明出处）。
//!
//! TODO(17)：填实现。加载 `dev.instagent/hooks.json`（回退草案位置
//! `hooks/hooks.json`）；matcher 是正则（regex crate），省略则每次都跑；
//! 命令 `sh -c` 跑、`${PLUGIN_ROOT}` 展开、默认超时 30s；环境变量透传白名单
//! （`PATH` `HOME` `LANG` + 插件声明的 `env`）；只有 PreToolUse 和 Stop 能
//! 阻止；loop 接线点在 `16`/`17`。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::plugin::PluginSet;

/// 默认超时 30s（§2.7）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Stop 连续阻止上限（goose 默认 8），防死循环。
pub const STOP_BLOCK_LIMIT: u32 = 8;

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

/// stdin 载荷 JSON（字段名照 goose HookContext）。
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

/// 无决策时怎么办（默认 allow）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    #[default]
    Allow,
    Block,
}

/// hooks.json 里一条 hook 定义（v1 只有 type = "command"）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDef {
    pub r#type: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: BTreeMap<HookEvent, Vec<HookMatcherGroup>>,
}

/// 单个 hook 及其来源插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredHook {
    pub plugin: String,
    pub event: HookEvent,
    pub group: HookMatcherGroup,
}

/// 决策：退出码 2 → Block(stderr)；stdout `{"decision":"block"}` → Block；
/// 退出 0 且空/allow → Allow；其余按 on_failure。
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
}

impl Hooks {
    /// 加载 + `${PLUGIN_ROOT}` 展开 + matcher 预编译。TODO(17)
    pub fn load(_plugins: &PluginSet) -> crate::Result<Hooks> {
        todo!("TODO(17)")
    }

    /// 跑该事件全部匹配 hook：任一 Block 即 Block；只有 PreToolUse / Stop
    /// 能阻止（其余事件忽略 Block）。TODO(17)
    pub async fn run(&self, _ctx: &HookContext) -> HookDecision {
        todo!("TODO(17)")
    }
}

/// 决策判定（参考 goose：`status.code() == Some(2)`、stdout JSON）。TODO(17)
pub fn parse_decision(
    _stdout: &str,
    _stderr: &str,
    _exit_code: Option<i32>,
    _on_failure: OnFailure,
) -> HookDecision {
    todo!("TODO(17)")
}
