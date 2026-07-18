use std::path::{Path, PathBuf};

use clap::Parser;
use radicle::identity::RepoId;

use crate::{home::ENVIRONMENT_VARIABLE, member::Member};

#[derive(Debug, Parser)]
#[command(version, about = "Run an AI contributor on Radicle")]
/// Command-line interface for the Radicle team application.
pub struct Cli {
    /// Team member to run: nico, maya, leon, vera, nora, or theo.
    member: Member,
    /// Radicle repository to watch.
    #[arg(short, long)]
    repo: RepoId,
    /// Application home containing isolated member data.
    #[arg(long, env = ENVIRONMENT_VARIABLE, value_name = "PATH")]
    home: Option<PathBuf>,
}

impl Cli {
    /// Parses the command-line arguments.
    #[must_use]
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Returns the selected team member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.member
    }

    /// Returns the repository to watch.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.repo
    }

    /// Returns the explicit application home, if configured.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use clap::{Parser as _, error::ErrorKind};
    use radicle::identity::RepoId;

    use crate::{cli::Cli, member::Member};

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[test]
    fn parses_every_member() {
        for member in Member::ALL {
            assert!(
                matches!(Cli::try_parse_from(["radicle-team", member.id(), "--repo", REPOSITORY]), Ok(cli) if cli.member() == member)
            );
        }
    }

    #[test]
    fn parses_repository() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "nico", "--repo", REPOSITORY]),
            Ok(cli) if cli.repository() == REPOSITORY.parse::<RepoId>().expect("repository ID should be valid")
        ));
    }

    #[test]
    fn parses_home() {
        assert!(matches!(
            Cli::try_parse_from([
                "radicle-team",
                "nico",
                "--repo",
                REPOSITORY,
                "--home",
                "/team"
            ]),
            Ok(cli) if cli.home() == Some(std::path::Path::new("/team"))
        ));
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "unknown", "--repo", REPOSITORY]),
            Err(error) if error.kind() == ErrorKind::ValueValidation
        ));
    }
}
