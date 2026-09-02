//! 5 个内置工具：shell / read / write / edit / tree（第三版 §1；描述文本从
//! goose `developer/mod.rs:108~186` 精简，搬运注明出处）。
//!
//! TODO(13)：填实现；经 `BuiltinTools: ToolSource`（id = `"builtin"`）注册，
//! 与其他来源同构。

pub mod fs;
pub mod shell;
pub mod tree;

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

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

#[async_trait]
impl ToolSource for BuiltinTools {
    fn id(&self) -> &str {
        "builtin"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        todo!("TODO(13)")
    }

    /// 按名字分发到 `shell::run` / `fs::*` / `tree::build_tree`。TODO(13)
    async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        todo!("TODO(13)")
    }
}
