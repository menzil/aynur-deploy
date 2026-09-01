use std::process::ExitCode;

use aynur_deploy::cli;

#[tokio::main]
async fn main() -> ExitCode {
    cli::main_entry().await
}
