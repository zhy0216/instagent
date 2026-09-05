//! Headless CLI process shell: initialize diagnostics and preserve task exit status.

mod cli;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    cli::init_logging();
    match cli::run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
