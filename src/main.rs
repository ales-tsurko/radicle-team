//! Runs a Radicle team member.

use std::process::ExitCode;

use radicle_team::cli::{Cli, LogBuffer};

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
    let logs = cli.interactive().then(LogBuffer::default);
    let log_file = cli.log_file().map(std::path::Path::to_path_buf);

    if let Err(error) = Logging::init(cli.log_level(), logs.clone(), log_file.as_deref()) {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    match cli.execute(logs).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(path) = log_file.as_deref()
                && let Err(error) = Logging::record_failure(path, &error)
            {
                eprintln!("Error: {error}");
            }
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
