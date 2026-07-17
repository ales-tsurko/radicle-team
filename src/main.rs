//! Runs a Radicle team member.

mod logging;

use std::process::ExitCode;

use radicle_team::{Application, cli::Cli};

fn main() -> ExitCode {
    let member = Cli::parse().member();

    if let Err(error) = logging::init() {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    Application::new(member).run();
    ExitCode::SUCCESS
}
