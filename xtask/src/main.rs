#![forbid(unsafe_code)]

use std::process::ExitCode;

mod eval;
mod perf;
mod providers;

#[tokio::main]
async fn main() -> ExitCode {
    providers::run().await
}
