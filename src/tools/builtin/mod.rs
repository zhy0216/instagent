//! 5 个内置工具：shell / read / write / edit / tree（第三版 §1；描述文本从
//! goose `developer/mod.rs:108~186`（commit `4ad43df`）精简搬运，搬运注明出处）。
//!
//! 以 [`BuiltinTools`]（id = `"builtin"`）注册进 Registry，与其他来源同构；
//! 内置工具名不加前缀。

pub mod fs;
pub mod shell;
pub mod tree;

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

use fs::READ_DEFAULT_LIMIT;

#[derive(Debug, Clone, Default)]
pub struct BuiltinTools {
    /// shell 用的 `$SHELL`，None = 运行时探测（配置 `shell` 字段）。
    pub shell: Option<PathBuf>,
}

impl BuiltinTools {
    pub fn new(shell: Option<PathBuf>) -> Self {
        Self { shell }
    }
}

#[derive(Debug, Deserialize)]
struct ShellInput {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    before: String,
    after: String,
}

#[derive(Debug, Deserialize)]
struct TreeInput {
    path: String,
    #[serde(default)]
    depth: Option<usize>,
}

fn spec(name: &str, description: &str, input_schema: Value, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        read_only,
    }
}

#[async_trait]
impl ToolSource for BuiltinTools {
    fn id(&self) -> &str {
        "builtin"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        vec![
            spec(
                "shell",
                // 精简搬运 goose developer/mod.rs:135~150（GOOSE_SHELL → INSTAGENT_SHELL，
                // 去掉 cmd.exe 分支；输出契约从 structured JSON 改为文本段）。
                "Execute a shell command in the session dir. Returns stdout, stderr and the \
                 exit code. The output of each stream is limited to up to 2000 lines / 50KB; \
                 longer outputs are truncated to a preview and the full text is saved to a \
                 temporary file whose path is returned.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Max seconds before the whole process group is killed (default 300)",
                        },
                    },
                    "required": ["command"],
                }),
                false,
            ),
            spec(
                "read",
                &format!(
                    "Read a file and return it with line numbers. Optionally start at `line` \
                     (1-based) and return at most `limit` lines (default {READ_DEFAULT_LIMIT})."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer", "description": "1-based start line" },
                        "limit": { "type": "integer" },
                    },
                    "required": ["path"],
                }),
                true,
            ),
            spec(
                // 搬运 goose developer/mod.rs:111。
                "write",
                "Create a new file or overwrite an existing file. Creates parent directories \
                 if needed.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                }),
                false,
            ),
            spec(
                // 搬运 goose developer/mod.rs:123。
                "edit",
                "Edit a file by finding and replacing text. The before text must match \
                 exactly and uniquely. Use empty after text to delete.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "before": { "type": "string" },
                        "after": { "type": "string" },
                    },
                    "required": ["path", "before", "after"],
                }),
                false,
            ),
            spec(
                // 搬运 goose developer/mod.rs:161~163。
                "tree",
                "List a directory tree with line counts. Traversal respects .gitignore rules.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "depth": {
                            "type": "integer",
                            "description": "Max depth, 0 means unlimited (default 2)",
                        },
                    },
                    "required": ["path"],
                }),
                true,
            ),
        ]
    }

    /// 按名字分发到 `shell::run` / `fs::*` / `tree::build_tree`。
    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput {
        match name {
            "shell" => match serde_json::from_value::<ShellInput>(input) {
                Ok(args) => {
                    shell::run(&args.command, args.timeout_secs, self.shell.as_deref(), ctx).await
                }
                Err(e) => ToolOutput::err(format!("Invalid input for shell: {e}")),
            },
            "read" => match serde_json::from_value::<ReadInput>(input) {
                Ok(args) => {
                    fs::read_file(std::path::Path::new(&args.path), args.line, args.limit, ctx)
                        .await
                }
                Err(e) => ToolOutput::err(format!("Invalid input for read: {e}")),
            },
            "write" => match serde_json::from_value::<WriteInput>(input) {
                Ok(args) => {
                    fs::write_file(std::path::Path::new(&args.path), &args.content, ctx).await
                }
                Err(e) => ToolOutput::err(format!("Invalid input for write: {e}")),
            },
            "edit" => match serde_json::from_value::<EditInput>(input) {
                Ok(args) => {
                    fs::edit_file(
                        std::path::Path::new(&args.path),
                        &args.before,
                        &args.after,
                        ctx,
                    )
                    .await
                }
                Err(e) => ToolOutput::err(format!("Invalid input for edit: {e}")),
            },
            "tree" => match serde_json::from_value::<TreeInput>(input) {
                Ok(args) => {
                    tree::build_tree(
                        std::path::Path::new(&args.path),
                        args.depth.unwrap_or(tree::DEFAULT_DEPTH),
                        ctx,
                    )
                    .await
                }
                Err(e) => ToolOutput::err(format!("Invalid input for tree: {e}")),
            },
            other => ToolOutput::err(format!("builtin: unknown tool `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Registry;
    use crate::tools::ToolCall;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn builtin_lists_five_tools_unprefixed() {
        let specs = BuiltinTools::new(None).list().await;
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["shell", "read", "write", "edit", "tree"]);
        let read_only: Vec<&str> = specs
            .iter()
            .filter(|s| s.read_only)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(read_only, vec!["read", "tree"]);
        for s in &specs {
            assert_eq!(s.input_schema["type"], "object");
        }
    }

    #[tokio::test]
    async fn builtin_registers_like_other_sources() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());

        let mut registry = Registry::new();
        registry.register(std::sync::Arc::new(BuiltinTools::new(None)));
        let specs = registry.list().await;
        assert_eq!(specs.len(), 5);

        let out = registry
            .call(
                &ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    input: json!({"path": "a/b.txt", "content": "hello"}),
                },
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert!(dir.path().join("a/b.txt").exists());

        let out = registry
            .call(
                &ToolCall {
                    id: "2".into(),
                    name: "read".into(),
                    input: json!({"path": "a/b.txt"}),
                },
                &ctx,
            )
            .await;
        assert_eq!(out.text, "   1: hello\n");
    }

    #[tokio::test]
    async fn builtin_rejects_bad_input_and_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let tools = BuiltinTools::new(None);

        let out = tools.call("read", json!({"nope": 1}), &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("Invalid input for read"));

        let out = tools.call("nope", json!({}), &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("unknown tool"));
    }
}
