//! Runs a Radicle team member.

use std::process::ExitCode;

use radicle_team::cli::Cli;

use crate::logging::Logging;

mod logging;

fn main() -> ExitCode {
    radicle_team::sandbox::dispatch_linux_helper();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to initialize asynchronous runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let cli = Cli::parse();

    if let Err(error) = Logging::init(cli.mode()) {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    match cli.execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(target: "radicle_team", %error, "application stopped");
            ExitCode::FAILURE
        }
    }
}
