//! Headless CLI process shell: initialize diagnostics and preserve task exit status.

mod cli;

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    cli::init_logging();
    let code = match cli::run().await {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(cli::output::stderr(), "error: {err:#}");
            ExitCode::FAILURE
        }
    };
    let _ = cli::output::stderr()
        .finish(Duration::from_millis(250))
        .await;
    code
}
