//! Headless 系统提示：自主执行任务、工具说明、`cwd` + 当前时间、输出契约。
//! MCP server instructions 与 skill 行由装配层传入。

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
        "You are instagent, a headless AI agent executing an unattended task. \
         No user is available during execution. Complete the supplied task autonomously: \
         do not ask follow-up questions, request approval, or wait for user input. \
         Make reasonable assumptions from the task and environment and state material \
         assumptions in the result. If essential information, credentials, permissions, \
         or capabilities are missing, explain why the task could not be completed and \
         never claim success.\n\n",
    );

    prompt.push_str("# Tools\n\n");
    if ctx.tools.is_empty() {
        prompt.push_str(
            "No tools are available for this task. Answer directly and say so when \
             you cannot inspect the environment.\n\n",
        );
    } else {
        prompt.push_str(
            "You can call the tools below to work with the task environment. Prefer a tool \
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
             when using the matching tools. If an instruction requires user interaction, \
             report the unmet prerequisite instead of waiting for a response.\n\n",
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
        "Follow the output format requested by the task, including plain text, JSON, or \
         another specified format. Keep the final result concise and focused on the task. \
         Describe the outcome and any unresolved blockers accurately; distinguish verified \
         results from assumptions. Do not offer to continue or end with a question.\n",
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
        assert!(prompt.contains("Follow the output format requested by the task"));
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

    #[test]
    fn unattended_task_contract_never_requires_a_reply_or_markdown() {
        let prompt = system(&PromptContext {
            tools: &[],
            cwd: Path::new("/tmp"),
            now: fixed_now(),
            mcp_instructions: &[],
            skill_lines: &[],
        });
        assert!(prompt.contains("headless AI agent executing an unattended task"));
        assert!(prompt
            .contains("do not ask follow-up questions, request approval, or wait for user input"));
        assert!(prompt.contains("Make reasonable assumptions"));
        assert!(prompt.contains("explain why the task could not be completed"));
        assert!(prompt.contains("never claim success"));
        assert!(prompt.contains("including plain text, JSON"));
        assert!(!prompt.contains("Use Markdown"));
    }
}
