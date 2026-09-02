//! 系统提示（第二版 §2.9）：`format!` 拼四段——身份一句话、工具说明、
//! `cwd` + 当前时间、响应规范（markdown、简洁）。
//!
//! 文本从 goose `crates/goose/src/prompts/system.md`（commit `4ad43df`）精简搬运：
//! 身份句改 goose→instagent（system.md:1~2），Extensions 段改为 MCP instructions
//! 注入位（system.md:10~30），Response Guidelines 沿用（system.md:37~39）。
//! 不用模板引擎；MCP server instructions 与 skill 行由 `18` 装配时传入。

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

/// 生成 system 字符串（请求字段，不是 Message）。
pub fn system(ctx: &PromptContext<'_>) -> String {
    let mut prompt = String::from(
        "You are instagent, a minimal general-purpose AI agent, developed as an \
         open-source project.\n\n",
    );

    prompt.push_str("# Tools\n\n");
    if ctx.tools.is_empty() {
        prompt.push_str(
            "No tools are available in this conversation. Answer directly and say so when \
             you cannot inspect the environment.\n\n",
        );
    } else {
        prompt.push_str(
            "You can call the tools below to work with the user's environment. Prefer a tool \
             call over guessing when you need facts about files or commands, and wait for \
             results before continuing.\n\n",
        );
        for tool in ctx.tools {
            prompt.push_str(&format!(
                "- {}: {}\n",
                tool.name,
                tool.description.replace('\n', " ")
            ));
        }
        prompt.push('\n');
    }

    if !ctx.mcp_instructions.is_empty() || !ctx.skill_lines.is_empty() {
        prompt.push_str("# Extensions & Skills\n\n");
        prompt.push_str(
            "Tool and skill behavior may be governed by the instructions below; follow them \
             when using the matching tools.\n\n",
        );
        for line in ctx.mcp_instructions {
            prompt.push_str(&format!("- {line}\n"));
        }
        for line in ctx.skill_lines {
            prompt.push_str(&format!("- skill {line}\n"));
        }
        prompt.push('\n');
    }

    prompt.push_str("# Environment\n\n");
    prompt.push_str(&format!(
        "Primary working directory: {}\nToday's date and time: {}\n\n",
        ctx.cwd.display(),
        ctx.now.format("%Y-%m-%d %H:%M UTC"),
    ));

    prompt.push_str("# Response Guidelines\n\n");
    prompt.push_str(
        "Use Markdown formatting for all responses. Keep answers concise and focused on what \
         the user asked.\n",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    #[test]
    fn assembles_all_four_sections() {
        let tools = vec![
            spec("shell", "run commands\nin a shell"),
            spec("read", "read files"),
        ];
        let mcp = vec!["fs server: always use relative paths".to_string()];
        let skills = vec!["pdf — read pdf documents".to_string()];
        let ctx = PromptContext {
            tools: &tools,
            cwd: Path::new("/work/demo"),
            now: fixed_now(),
            mcp_instructions: &mcp,
            skill_lines: &skills,
        };
        let prompt = system(&ctx);

        assert!(prompt.starts_with("You are instagent"), "{prompt}");
        assert!(prompt.contains("# Tools"));
        assert!(prompt.contains("- shell: run commands in a shell"));
        assert!(prompt.contains("- read: read files"));
        assert!(prompt.contains("# Extensions & Skills"));
        assert!(prompt.contains("- fs server: always use relative paths"));
        assert!(prompt.contains("- skill pdf — read pdf documents"));
        assert!(prompt.contains("Primary working directory: /work/demo"));
        assert!(prompt.contains("Today's date and time: 2026-01-02 03:04 UTC"));
        assert!(prompt.contains("Use Markdown formatting"));
    }

    #[test]
    fn empty_tools_and_no_injection_slots() {
        let ctx = PromptContext {
            tools: &[],
            cwd: Path::new("/tmp"),
            now: fixed_now(),
            mcp_instructions: &[],
            skill_lines: &[],
        };
        let prompt = system(&ctx);
        assert!(prompt.contains("No tools are available"));
        assert!(!prompt.contains("# Extensions & Skills"));
    }
}
