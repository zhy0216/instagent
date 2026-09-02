//! 插件斜杠命令（第三版 §2.8）：`dev.instagent/commands/*.md`，Claude Code
//! 约定，约 60 行。
//!
//! TODO(17)：填实现。`/review security` 展开成一条用户消息；列表与展开接口
//! 给 `18` 的 REPL。

use crate::plugin::PluginSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// 命令名 = 文件名（不含扩展名）。
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// 正文模板，`$ARGUMENTS` 展开。
    pub template: String,
}

/// frontmatter：description / argument-hint。TODO(17)
pub fn load_commands(_plugins: &PluginSet) -> crate::Result<Vec<SlashCommand>> {
    todo!("TODO(17)")
}

/// 命令正文 + 用户参数 → 一条用户消息文本。TODO(17)
pub fn expand(_cmd: &SlashCommand, _args: &str) -> String {
    todo!("TODO(17)")
}
