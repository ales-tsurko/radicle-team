//! Team member filesystem locations.

use std::{env, path::PathBuf};

use directories::BaseDirs;

use crate::member::Member;

/// Environment variable overriding the application home directory.
pub const ENVIRONMENT_VARIABLE: &str = "RADICLE_TEAM_HOME";

/// Error resolving the application home directory.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The operating-system home directory could not be determined.
    #[error("could not determine the user home directory; set {ENVIRONMENT_VARIABLE}")]
    HomeUnavailable,
}

/// Filesystem locations owned by one team member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHome {
    path: PathBuf,
}

impl MemberHome {
    /// Resolves the selected member's home directory.
    ///
    /// Uses [`ENVIRONMENT_VARIABLE`] when set, otherwise defaults to `~/.radicle-team`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating-system home directory is unavailable.
    pub fn discover(member: Member) -> Result<Self, Error> {
        let root = env::var_os(ENVIRONMENT_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                BaseDirs::new().map(|directories| directories.home_dir().join(".radicle-team"))
            })
            .ok_or(Error::HomeUnavailable)?;

        Ok(Self::at(root, member))
    }

    /// Creates member locations below an explicit application root.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>, member: Member) -> Self {
        Self {
            path: root.into().join("members").join(member.id()),
        }
    }

    /// Returns the member directory.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns the member's Radicle home directory.
    #[must_use]
    pub fn radicle(&self) -> PathBuf {
        self.path.join("radicle")
    }

    /// Returns the member's watcher state database path.
    #[must_use]
    pub fn state_database(&self) -> PathBuf {
        self.path.join("state.db")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{home::MemberHome, member::Member};

    #[test]
    fn creates_isolated_member_locations() {
        let home = MemberHome::at("/team", Member::Nico);

        assert_eq!(home.path(), Path::new("/team/members/nico"));
        assert_eq!(home.radicle(), Path::new("/team/members/nico/radicle"));
        assert_eq!(
            home.state_database(),
            Path::new("/team/members/nico/state.db")
        );
    }
}
