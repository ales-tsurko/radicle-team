//! Library for running an AI contributor on Radicle.

/// A single Radicle team member.
#[derive(Debug, Default)]
pub struct Application;

impl Application {
    /// Creates the application.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Runs the team member.
    pub fn run(&self) {
        tracing::info!("team member initialized");
    }
}
