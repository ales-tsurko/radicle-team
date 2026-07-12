//! Runs a Radicle team member.

mod logging;

use std::process::ExitCode;

use radicle_team::Application;

fn main() -> ExitCode {
    if let Err(error) = logging::init() {
        eprintln!("failed to initialize logging: {error}");
        return ExitCode::FAILURE;
    }

    Application::new().run();
    ExitCode::SUCCESS
}
