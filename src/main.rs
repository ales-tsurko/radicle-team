//! Runs a Radicle team member.

use std::process::ExitCode;

use radicle_team::{Application, cli::Cli};

use crate::logging::Logging;

mod logging;

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(error) = Logging::init() {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    let mut application = Application::new(cli.member(), cli.repository());
    if let Some(home) = cli.home() {
        application = application.with_home(home);
    }

    match application.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "application stopped");
            ExitCode::FAILURE
        }
    }
}
