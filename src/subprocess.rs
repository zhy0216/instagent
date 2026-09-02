//! 子进程：一律进程组 + `kill_on_drop(true)`，防 Ctrl-C 后残留（第二版 §2.12）。
//!
//! TODO(03)：从 `~/yyds/goose/crates/goose/src/subprocess.rs`（144 行）移植
//! `configure_subprocess` / `spawn_long_lived_mcp_subprocess`（含 Linux
//! PR_SET_PDEATHSIG 特判），commit message 注明出处。shell / MCP stdio /
//! hooks / proxy 都复用这里。

use std::io;

use rmcp::transport::TokioChildProcess;
use tokio::process::ChildStderr;
use tokio::process::Command;

/// 进程组隔离 + `kill_on_drop(true)` +（Linux）parent-death 信号。TODO(03)
pub fn configure_subprocess(_command: &mut Command) {
    todo!("TODO(03)")
}

/// 长驻 MCP stdio 子进程：返回 transport 与被管道化的 stderr（stderr 接日志）。TODO(03)
pub async fn spawn_long_lived_mcp_subprocess(
    _command: Command,
) -> io::Result<(TokioChildProcess, Option<ChildStderr>)> {
    todo!("TODO(03)")
}

/// 加固过的 git 命令（拒绝隐式 bare repo、禁 fsmonitor hook），`07` 安装用。TODO(03)
pub fn git_command() -> std::process::Command {
    todo!("TODO(03)")
}
