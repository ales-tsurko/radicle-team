//! Team member filesystem locations.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{env, fs, io, path::PathBuf};

use directories::BaseDirs;

use crate::member::Member;

/// Environment variable overriding the application home directory.
pub const ENVIRONMENT_VARIABLE: &str = "RADICLE_TEAM_HOME";

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

    /// Creates the member directory with private permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or secured.
    pub fn create(&self) -> Result<(), io::Error> {
        fs::create_dir_all(&self.path)?;

        #[cfg(unix)]
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700))?;

        Ok(())
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

    /// Returns the member process lock path.
    #[must_use]
    pub fn process_lock(&self) -> PathBuf {
        self.path.join("process.lock")
    }

    /// Returns the managed Radicle node log path.
    #[must_use]
    pub fn node_log(&self) -> PathBuf {
        self.path.join("node.log")
    }

    /// Returns the root for ephemeral agent working copies.
    #[must_use]
    pub fn workspaces(&self) -> PathBuf {
        self.path.join("workspaces")
    }

    /// Returns the local agent sandbox configuration path.
    #[must_use]
    pub fn sandbox_config(&self) -> PathBuf {
        self.path.join("sandbox.toml")
    }
}

/// Error resolving the application home directory.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The operating-system home directory could not be determined.
    #[error("could not determine the user home directory; set {ENVIRONMENT_VARIABLE}")]
    HomeUnavailable,
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
        assert_eq!(
            home.process_lock(),
            Path::new("/team/members/nico/process.lock")
        );
        assert_eq!(home.node_log(), Path::new("/team/members/nico/node.log"));
        assert_eq!(
            home.workspaces(),
            Path::new("/team/members/nico/workspaces")
        );
        assert_eq!(
            home.sandbox_config(),
            Path::new("/team/members/nico/sandbox.toml")
        );
    }
}
