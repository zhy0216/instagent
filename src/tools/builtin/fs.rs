//! read / write / edit 工具（第二版 §2.4；edit 参考 goose
//! `developer/edit.rs:157~286`，搬运注明出处）。
//!
//! TODO(13)：填实现。

use std::path::Path;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// read 默认最多 2000 行。
pub const READ_DEFAULT_LIMIT: u32 = 2000;

/// 带行号输出（read 标 read_only = true）。TODO(13)
pub async fn read_file(
    _path: &Path,
    _line: Option<u32>,
    _limit: Option<u32>,
    _ctx: &ToolCtx,
) -> ToolOutput {
    todo!("TODO(13)")
}

/// 建父目录，覆盖写。TODO(13)
pub async fn write_file(_path: &Path, _content: &str, _ctx: &ToolCtx) -> ToolOutput {
    todo!("TODO(13)")
}

/// `before` 必须唯一精确匹配，否则报错并给出匹配次数和相近上下文；
/// `after` 为空即删除。TODO(13)
pub async fn edit_file(_path: &Path, _before: &str, _after: &str, _ctx: &ToolCtx) -> ToolOutput {
    todo!("TODO(13)")
}
