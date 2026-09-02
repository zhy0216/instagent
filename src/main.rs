//! CLI 入口（A4；第二版 §2.11）。`18`：clap 定义、子命令分发与运行时装配
//! 都在 `src/cli/**`；本文件只做进程壳（日志初始化 + 顶层错误打印）。

mod cli;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    cli::init_logging();
    match cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
