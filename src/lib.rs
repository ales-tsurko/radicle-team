//! Library for running an AI contributor on Radicle.

use crate::member::Member;

/// Command-line interface for selecting and running a team member.
pub mod cli;
/// Fixed team members and their profiles.
pub mod member;

/// A single Radicle team member application.
#[derive(Debug)]
pub struct Application {
    member: Member,
}

impl Application {
    /// Creates an application for `member`.
    #[must_use]
    pub const fn new(member: Member) -> Self {
        Self { member }
    }

    /// Returns the selected member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.member
    }

    /// Runs the team member.
    pub fn run(&self) {
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            "team member initialized"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_retains_selected_member() {
        assert_eq!(Application::new(Member::Vera).member(), Member::Vera);
    }
}
