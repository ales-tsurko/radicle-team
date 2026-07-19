use std::path::{Path, PathBuf};

use clap::Parser;
use radicle::identity::{Did, RepoId};

use crate::{
    Application,
    home::ENVIRONMENT_VARIABLE,
    identity::{KeyProtection, Provider},
    member::Member,
};

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
    /// Use a dedicated existing Radicle home instead of a managed identity.
    #[arg(long, value_name = "PATH", conflicts_with = "unencrypted_identity")]
    identity_home: Option<PathBuf>,
    /// Require an existing Radicle home to derive this DID.
    #[arg(long, requires = "identity_home", value_name = "DID")]
    expected_did: Option<Did>,
    /// Create a permissions-protected managed key without encryption.
    #[arg(long)]
    unencrypted_identity: bool,
    /// Use an explicit project configuration file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
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

    /// Returns the selected identity provider.
    #[must_use]
    pub fn identity_provider(&self) -> Provider {
        match &self.identity_home {
            Some(home) => Provider::RadicleHome {
                home: home.clone(),
                expected_did: self.expected_did,
            },
            None => Provider::Managed {
                protection: if self.unencrypted_identity {
                    KeyProtection::Unencrypted
                } else {
                    KeyProtection::SystemCredential
                },
            },
        }
    }

    /// Returns the explicit project configuration file, if configured.
    #[must_use]
    pub fn config_file(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    /// Builds the configured application.
    #[must_use]
    pub fn application(self) -> Application {
        let mut application = Application::new(self.member, self.repo)
            .with_identity_provider(self.identity_provider());
        if let Some(home) = self.home {
            application = application.with_home(home);
        }
        if let Some(config) = self.config {
            application = application.with_config_file(config);
        }

        application
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
            assert!(matches!(
                Cli::try_parse_from(["radicle-team", member.id(), "--repo", REPOSITORY]),
                Ok(cli) if cli.member() == member
            ));
        }
    }

    #[test]
    fn parses_repository() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "nico", "--repo", REPOSITORY]),
            Ok(cli)
                if cli.repository()
                    == REPOSITORY
                        .parse::<RepoId>()
                        .expect("repository ID should be valid")
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
    fn configures_an_existing_identity() {
        const DID: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
        let cli = Cli::try_parse_from([
            "radicle-team",
            "nico",
            "--repo",
            REPOSITORY,
            "--identity-home",
            "/radicle",
            "--expected-did",
            DID,
            "--config",
            "/project.toml",
        ])
        .expect("CLI should parse");

        assert!(matches!(
            cli.identity_provider(),
            crate::identity::Provider::RadicleHome { home, expected_did }
                if home == std::path::Path::new("/radicle")
                    && expected_did == Some(DID.parse().expect("DID should parse"))
        ));
        assert_eq!(
            cli.config_file(),
            Some(std::path::Path::new("/project.toml"))
        );
    }

    #[test]
    fn configures_an_unencrypted_managed_identity() {
        let cli = Cli::try_parse_from([
            "radicle-team",
            "nico",
            "--repo",
            REPOSITORY,
            "--unencrypted-identity",
        ])
        .expect("CLI should parse");

        assert_eq!(
            cli.identity_provider(),
            crate::identity::Provider::Managed {
                protection: crate::identity::KeyProtection::Unencrypted,
            }
        );
    }

    #[test]
    fn expected_did_requires_existing_identity_home() {
        assert!(matches!(
            Cli::try_parse_from([
                "radicle-team",
                "nico",
                "--repo",
                REPOSITORY,
                "--expected-did",
                "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            ]),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument
        ));
    }

    #[test]
    fn builds_the_application() {
        let application = Cli::try_parse_from([
            "radicle-team",
            "nico",
            "--repo",
            REPOSITORY,
            "--home",
            "/team",
            "--config",
            "/project.toml",
        ])
        .expect("CLI should parse")
        .application();

        assert_eq!(application.member(), Member::Nico);
        assert_eq!(application.home(), Some(std::path::Path::new("/team")));
        assert_eq!(
            application.config_file(),
            Some(std::path::Path::new("/project.toml"))
        );
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "unknown", "--repo", REPOSITORY]),
            Err(error) if error.kind() == ErrorKind::ValueValidation
        ));
    }
}
