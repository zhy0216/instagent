//! tree 工具（第二版 §2.4；参考 goose `developer/tree.rs`）。
//!
//! TODO(13)：填实现。目录树 + 行数，用 `ignore` crate 遵守 .gitignore；
//! 标 read_only = true。

use std::path::Path;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

pub const DEFAULT_DEPTH: usize = 2;

/// 渲染 `path` 下最深 `depth` 层的目录树。TODO(13)
pub async fn build_tree(_path: &Path, _depth: usize, _ctx: &ToolCtx) -> ToolOutput {
    todo!("TODO(13)")
}
