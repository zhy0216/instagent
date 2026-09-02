//! 系统提示（第二版 §2.9）：`format!` 拼四段——身份一句话、工具说明、
//! `cwd` + 当前时间、响应规范（markdown、简洁）。文本从 goose
//! `crates/goose/src/prompts/system.md` 精简（搬运注明出处）。不用模板引擎。
//!
//! TODO(16)：填 `system`。MCP server 的 `instructions` 与全部 skill 的
//! name + description 由 `18` 装配时传入注入位。

use std::path::Path;

use chrono::DateTime;
use chrono::Utc;

use crate::tools::ToolSpec;

/// 拼装输入；切片是借用，避免副本。
pub struct PromptContext<'a> {
    pub tools: &'a [ToolSpec],
    pub cwd: &'a Path,
    pub now: DateTime<Utc>,
    /// 每条 = 一个 MCP server 的 instructions（含 server 名前缀）。
    pub mcp_instructions: &'a [String],
    /// 每条 = 一个 skill 的 `name — description`。
    pub skill_lines: &'a [String],
}

/// 生成 system 字符串（请求字段，不是 Message）。TODO(16)
pub fn system(_ctx: &PromptContext<'_>) -> String {
    todo!("TODO(16)")
}
