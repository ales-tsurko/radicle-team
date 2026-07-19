//! Runs a Radicle team member.

use std::process::ExitCode;

use radicle_team::cli::Cli;

use crate::logging::Logging;

mod logging;

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(error) = Logging::init() {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    let application = cli.application();

    match application.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "application stopped");
            ExitCode::FAILURE
        }
    }
}
