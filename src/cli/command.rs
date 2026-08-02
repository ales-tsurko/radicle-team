//! Argument parsing and command dispatch.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use radicle::{
    identity::{Did, RepoId},
    node::NodeId,
};

use crate::{
    Application,
    cli::setup::{Error as SetupError, SetupWizard},
    home::ENVIRONMENT_VARIABLE,
    identity::{KeyProtection, Provider},
    member::Member,
    shutdown::Shutdown,
};

#[derive(Debug, Parser)]
#[command(version, about = "Run an AI contributor on Radicle")]
/// Command-line interface for the Radicle team application.
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// Parses the command-line arguments.
    #[must_use]
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Returns the requested operation.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        match self.command {
            Command::Setup(_) => Mode::Setup,
            Command::Run(_) => Mode::Run,
        }
    }

    /// Returns the selected team member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.options().member
    }

    /// Returns the repository to use.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.options().repo
    }

    /// Returns the explicit application home, if configured.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.options().home.as_deref()
    }

    /// Returns the selected identity provider.
    #[must_use]
    pub fn identity_provider(&self) -> Provider {
        self.options().identity_provider()
    }

    /// Returns the explicit repository manifest, if configured.
    #[must_use]
    pub fn manifest_file(&self) -> Option<&Path> {
        self.options().manifest.as_deref()
    }

    /// Builds the configured application.
    #[must_use]
    pub fn application(self) -> Application {
        let options = self.command.into_options();
        let mut application = Application::new(options.member, options.repo)
            .with_identity_provider(options.identity_provider());
        if let Some(home) = options.home {
            application = application.with_home(home);
        }
        if let Some(manifest) = options.manifest {
            application = application.with_manifest_file(manifest);
        }
        for seed in options.seeds {
            application = application.with_seed(seed);
        }

        application
    }

    /// Executes the selected command.
    ///
    /// # Errors
    ///
    /// Returns an error when setup or member execution fails.
    pub async fn execute(self) -> Result<(), Error> {
        let mode = self.mode();
        let shutdown = Shutdown::install()?;
        let application = self.application().with_shutdown(shutdown);
        match mode {
            Mode::Setup => SetupWizard::new(application)
                .run()
                .await
                .map_err(Error::from),
            Mode::Run => Box::pin(application.run()).await.map_err(Error::from),
        }
    }

    const fn options(&self) -> &Options {
        self.command.options()
    }
}

/// Error executing a CLI command.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Application execution failed.
    #[error(transparent)]
    Application(Box<crate::Error>),
    /// Interactive setup failed.
    #[error(transparent)]
    Setup(Box<SetupError>),
    /// Installing process signal handlers failed.
    #[error("failed to install process signal handlers: {0}")]
    Signal(#[from] ctrlc::Error),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Application(Box::new(error))
    }
}

impl From<SetupError> for Error {
    fn from(error: SetupError) -> Self {
        Self::Setup(Box::new(error))
    }
}

/// Operation selected on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Prepare an identity and repository.
    Setup,
    /// Run the member activity watcher.
    Run,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Prepare a member identity and repository.
    Setup(Options),
    /// Run a member.
    Run(Options),
}

impl Command {
    const fn options(&self) -> &Options {
        match self {
            Self::Setup(options) | Self::Run(options) => options,
        }
    }

    fn into_options(self) -> Options {
        match self {
            Self::Setup(options) | Self::Run(options) => options,
        }
    }
}

#[derive(Debug, Args)]
struct Options {
    /// Team member to select: nico, maya, leon, vera, nora, or theo.
    member: Member,
    /// Radicle repository to prepare or watch.
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
    /// Use an explicit repository manifest.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,
    /// Prefer this seed while synchronizing. May be repeated.
    #[arg(long = "seed", value_name = "NID")]
    seeds: Vec<NodeId>,
}

impl Options {
    fn identity_provider(&self) -> Provider {
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
}

#[cfg(test)]
mod tests {
    use clap::{Parser as _, error::ErrorKind};
    use radicle::identity::RepoId;

    use crate::{
        cli::{Cli, Mode},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[test]
    fn parses_every_member() {
        for member in Member::ALL {
            assert!(matches!(
                Cli::try_parse_from(["radicle-team", "run", member.id(), "--repo", REPOSITORY]),
                Ok(cli) if cli.member() == member
            ));
        }
    }

    #[test]
    fn parses_repository() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "run", "nico", "--repo", REPOSITORY]),
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
                "run",
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
            "run",
            "nico",
            "--repo",
            REPOSITORY,
            "--identity-home",
            "/radicle",
            "--expected-did",
            DID,
            "--manifest",
            "/manifest.toml",
        ])
        .expect("CLI should parse");

        assert!(matches!(
            cli.identity_provider(),
            crate::identity::Provider::RadicleHome { home, expected_did }
                if home == std::path::Path::new("/radicle")
                    && expected_did == Some(DID.parse().expect("DID should parse"))
        ));
        assert_eq!(
            cli.manifest_file(),
            Some(std::path::Path::new("/manifest.toml"))
        );
    }

    #[test]
    fn configures_an_unencrypted_managed_identity() {
        let cli = Cli::try_parse_from([
            "radicle-team",
            "run",
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
                "run",
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
            "run",
            "nico",
            "--repo",
            REPOSITORY,
            "--home",
            "/team",
            "--manifest",
            "/manifest.toml",
        ])
        .expect("CLI should parse")
        .application();

        assert_eq!(application.member(), Member::Nico);
        assert_eq!(application.home(), Some(std::path::Path::new("/team")));
        assert_eq!(
            application.manifest_file(),
            Some(std::path::Path::new("/manifest.toml"))
        );
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "run", "unknown", "--repo", REPOSITORY]),
            Err(error) if error.kind() == ErrorKind::ValueValidation
        ));
    }

    #[test]
    fn parses_setup_mode_and_seeds() {
        const SEED: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
        let cli = Cli::try_parse_from([
            "radicle-team",
            "setup",
            "nico",
            "--repo",
            REPOSITORY,
            "--seed",
            SEED,
        ])
        .expect("setup CLI should parse");

        assert_eq!(cli.mode(), Mode::Setup);
        assert_eq!(cli.application().seeds().len(), 1);
    }
}
