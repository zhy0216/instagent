//! shell 工具（第二版 §2.4）。
//!
//! TODO(13)：填实现。`$SHELL -c` 或 `bash -c`，cwd 为会话目录，用 `03` 的
//! 进程组 + kill_on_drop；超时或取消时 kill 整组。输出截断与渲染参考 goose
//! `developer/shell.rs` 的常量与 `render_output` / `save_full_output` 组
//! （搬运在 commit message 注明出处）。

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// 默认超时（第二版 §2.4）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// 每流上限：2000 行 / 50KB。
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
/// 超出时给前 50 行 / 10KB 预览，全文存临时文件并返回路径。
pub const PREVIEW_LINES: usize = 50;
pub const PREVIEW_BYTES: usize = 10 * 1024;

/// 跑一条命令，返回 stdout、stderr、exit code 的组合格式（is_error = 非零退出码）。
/// TODO(13)
pub async fn run(_command: &str, _timeout_secs: Option<u64>, _ctx: &ToolCtx) -> ToolOutput {
    todo!("TODO(13)")
}

/// 截断 + 拼装输出（参考 goose developer/shell.rs:868 render_output）。TODO(13)
pub fn render_output(_stdout: &str, _stderr: &str, _exit_code: Option<i32>) -> String {
    todo!("TODO(13)")
}
