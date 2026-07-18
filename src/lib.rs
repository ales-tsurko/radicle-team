//! Library for running an AI contributor on Radicle.

use std::path::{Path, PathBuf};

use radicle::identity::RepoId;

use crate::member::Member;

/// Command-line interface for selecting and running a team member.
pub mod cli;
/// Filesystem locations used by each team member.
pub mod home;
/// Fixed team members and their profiles.
pub mod member;
/// Read-only Radicle activity watching.
pub mod watcher;

/// A single Radicle team member application.
#[derive(Debug)]
pub struct Application {
    member: Member,
    repository: RepoId,
    home: Option<PathBuf>,
}

impl Application {
    /// Creates an application for `member` watching `repository`.
    #[must_use]
    pub const fn new(member: Member, repository: RepoId) -> Self {
        Self {
            member,
            repository,
            home: None,
        }
    }

    /// Uses an explicit application home directory.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Returns the selected member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.member
    }

    /// Returns the watched repository.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.repository
    }

    /// Returns the explicit application home, if configured.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Runs the team member.
    ///
    /// # Errors
    ///
    /// Returns an error if the Radicle profile, node, repository, event stream,
    /// or local watcher state cannot be accessed.
    pub fn run(&self) -> Result<(), watcher::Error> {
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            "team member initialized"
        );
        let mut watcher = watcher::Watcher::new(self.member, self.repository);
        if let Some(home) = &self.home {
            watcher = watcher.with_home(home);
        }
        watcher.run(|activity| {
            tracing::info!(?activity, "activity received");
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use radicle::identity::RepoId;

    use crate::{Application, member::Member};

    #[test]
    fn application_retains_selection() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let application = Application::new(Member::Vera, repository);

        assert_eq!(application.member(), Member::Vera);
        assert_eq!(application.repository(), repository);
        assert_eq!(application.home(), None);
    }

    #[test]
    fn application_accepts_explicit_home() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let application = Application::new(Member::Vera, repository).with_home("/team");

        assert_eq!(application.home(), Some(std::path::Path::new("/team")));
    }
}
