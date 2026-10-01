//! Argument parsing and command dispatch.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use radicle::{
    identity::{Did, RepoId},
    node::NodeId,
};

use crate::{
    Application,
    cli::{
        provider as provider_command,
        setup::{Error as SetupError, SetupWizard},
    },
    home::ENVIRONMENT_VARIABLE,
    identity::{KeyProtection, Provider},
    member::Member,
    shutdown::Shutdown,
};

#[derive(Debug, Parser)]
#[command(name = "rad-team", version, about = "Run an AI contributor on Radicle")]
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
            Command::Providers(_) => Mode::Providers,
            Command::Login(_) => Mode::Login,
        }
    }

    /// Returns the selected team member, for the commands which run one.
    #[must_use]
    pub fn member(&self) -> Option<Member> {
        self.options().map(|options| options.member)
    }

    /// Returns the repository to use, for the commands which need one.
    #[must_use]
    pub fn repository(&self) -> Option<RepoId> {
        match &self.command {
            Command::Setup(options) => options.repo,
            Command::Run(options) => Some(options.repo),
            Command::Providers(_) | Command::Login(_) => None,
        }
    }

    /// Returns the explicit application home, if configured.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.options()?.home.as_deref()
    }

    /// Returns the selected identity provider, for the commands which load one.
    #[must_use]
    pub fn identity_provider(&self) -> Option<Provider> {
        self.options().map(Options::identity_provider)
    }

    /// Returns the explicit repository manifest, if configured.
    #[must_use]
    pub fn manifest_file(&self) -> Option<&Path> {
        self.options()?.manifest.as_deref()
    }

    /// Builds the configured application.
    ///
    /// # Errors
    ///
    /// Returns an error when `setup` was given no directory and the working directory cannot be
    /// read, or identity-only setup has no repository to build an application for.
    pub fn application(self) -> Result<Application, Error> {
        let mode = self.mode();
        let repository = self.repository();
        let options = self
            .command
            .into_options()
            .ok_or(Error::NotAMemberCommand)?;
        let repository = repository.ok_or(Error::RepositoryRequired)?;
        let mut application = Application::new(options.member, repository)
            .with_identity_provider(options.identity_provider());
        match (options.home, mode) {
            (Some(home), _) => application = application.with_home(home),
            (None, Mode::Setup) => {
                application = application.with_home(
                    Application::default_setup_home(options.member, Some(repository))?.path(),
                );
            }
            (None, _) => {}
        }
        if let Some(manifest) = options.manifest {
            application = application.with_manifest_file(manifest);
        }
        if let Some(provider) = options.provider {
            application = application.with_provider(provider);
        }
        for seed in options.seeds {
            application = application.with_seed(seed);
        }

        Ok(application)
    }

    /// Executes the selected command.
    ///
    /// # Errors
    ///
    /// Returns an error when setup or member execution fails.
    pub async fn execute(self) -> Result<(), Error> {
        match self.command {
            Command::Providers(options) => {
                provider_command::list_configured(options.home).map_err(Error::from)
            }
            Command::Login(options) => {
                provider_command::login_configured(&options.provider, options.home)
                    .map_err(Error::from)
            }
            command @ (Command::Setup(_) | Command::Run(_)) => {
                Box::pin(Self { command }.run_member()).await
            }
        }
    }

    // Runs the commands which need a member identity and a repository.
    async fn run_member(self) -> Result<(), Error> {
        let prepare = self.mode() == Mode::Setup;
        let shutdown = Shutdown::install()?;
        if let Command::Setup(options) = &self.command
            && options.identity_only
        {
            let member = options.member.member;
            let home = match &options.member.home {
                Some(home) => crate::home::MemberHome::at(home),
                None => Application::default_setup_home(member, options.repo)?,
            };
            let did = Application::provision_identity(
                member,
                options.repo,
                home,
                options.member.identity_provider(),
            )
            .await?;
            println!("{member} identity: {did}");
            return Ok(());
        }
        let application = self.application()?.with_shutdown(shutdown);
        if prepare {
            return SetupWizard::new(application)
                .run()
                .await
                .map_err(Error::from);
        }

        Box::pin(application.run()).await.map_err(Error::from)
    }

    const fn options(&self) -> Option<&Options> {
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
    /// The member directory could not be resolved.
    #[error(transparent)]
    Home(#[from] crate::home::Error),
    /// Listing or authorizing a model provider failed.
    #[error(transparent)]
    Provider(Box<provider_command::Error>),
    /// A command which runs no member was asked for one.
    #[error("this command does not run a team member")]
    NotAMemberCommand,
    /// A repository operation was requested without selecting a repository.
    #[error("--repo is required for full setup or run")]
    RepositoryRequired,
}

impl From<provider_command::Error> for Error {
    fn from(error: provider_command::Error) -> Self {
        Self::Provider(Box::new(error))
    }
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
    /// List the model providers this machine has.
    Providers,
    /// Authorize a model provider.
    Login,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Prepare a member identity and repository.
    Setup(SetupOptions),
    /// Run a member.
    Run(RunOptions),
    /// List the model providers this machine can run members on.
    Providers(ProvidersOptions),
    /// Authorize a model provider.
    Login(LoginOptions),
}

impl Command {
    const fn options(&self) -> Option<&Options> {
        match self {
            Self::Setup(options) => Some(&options.member),
            Self::Run(options) => Some(&options.member),
            Self::Providers(_) | Self::Login(_) => None,
        }
    }

    fn into_options(self) -> Option<Options> {
        match self {
            Self::Setup(options) => Some(options.member),
            Self::Run(options) => Some(options.member),
            Self::Providers(_) | Self::Login(_) => None,
        }
    }
}

#[derive(Debug, Args)]
struct SetupOptions {
    #[command(flatten)]
    member: Options,
    /// Repository to prepare; optional when only provisioning an identity.
    #[arg(short, long, required_unless_present = "identity_only")]
    repo: Option<RepoId>,
    /// Save the member identity and print its DID without starting a node or fetching.
    #[arg(long)]
    identity_only: bool,
}

#[derive(Debug, Args)]
struct RunOptions {
    #[command(flatten)]
    member: Options,
    /// Radicle repository to watch.
    #[arg(short, long)]
    repo: RepoId,
}

#[derive(Debug, Args)]
struct ProvidersOptions {
    // Also report what this member alone was authorized with.
    //
    // The same store `login --home` writes to. Without it the listing sees only the machine-wide
    // one, and a member-scoped login reads as never having happened.
    #[arg(long, value_name = "PATH")]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct LoginOptions {
    // Provider to authorize, as listed by `providers`.
    provider: String,
    // Authorize this member alone rather than everything on the machine.
    //
    // Deliberately not read from the environment, unlike the member commands. An exported member
    // home would otherwise redirect a machine-wide login into one member's store, which nothing
    // else looks in.
    #[arg(long, value_name = "PATH")]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Options {
    // Team member to select: nico, maya, leon, vera, nora, or theo.
    member: Member,
    // Application home containing isolated member data.
    #[arg(long, env = ENVIRONMENT_VARIABLE, value_name = "PATH")]
    home: Option<PathBuf>,
    // Use a dedicated existing Radicle home instead of a managed identity.
    #[arg(long, value_name = "PATH", conflicts_with = "unencrypted_identity")]
    identity_home: Option<PathBuf>,
    // Require an existing Radicle home to derive this DID.
    #[arg(long, requires = "identity_home", value_name = "DID")]
    expected_did: Option<Did>,
    // Create a permissions-protected managed key without encryption.
    #[arg(long)]
    unencrypted_identity: bool,
    // Run on this model provider instead of the one the manifest names.
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,
    // Use an explicit repository manifest.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,
    // Prefer this seed while synchronizing. May be repeated.
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
                Cli::try_parse_from(["rad-team", "run", member.id(), "--repo", REPOSITORY]),
                Ok(cli) if cli.member() == Some(member)
            ));
        }
    }

    #[test]
    fn parses_repository() {
        assert!(matches!(
            Cli::try_parse_from(["rad-team", "run", "nico", "--repo", REPOSITORY]),
            Ok(cli)
                if cli.repository()
                    == Some(
                        REPOSITORY
                            .parse::<RepoId>()
                            .expect("repository ID should be valid")
                    )
        ));
    }

    #[test]
    fn parses_home() {
        assert!(matches!(
            Cli::try_parse_from([
                "rad-team",
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
            "rad-team",
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
            Some(crate::identity::Provider::RadicleHome { home, expected_did })
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
            "rad-team",
            "run",
            "nico",
            "--repo",
            REPOSITORY,
            "--unencrypted-identity",
        ])
        .expect("CLI should parse");

        assert_eq!(
            cli.identity_provider(),
            Some(crate::identity::Provider::Managed {
                protection: crate::identity::KeyProtection::Unencrypted,
            })
        );
    }

    #[test]
    fn expected_did_requires_existing_identity_home() {
        assert!(matches!(
            Cli::try_parse_from([
                "rad-team",
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
            "rad-team",
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

        let application = application.expect("application should build");
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
            Cli::try_parse_from(["rad-team", "run", "unknown", "--repo", REPOSITORY]),
            Err(error) if error.kind() == ErrorKind::ValueValidation
        ));
    }

    #[test]
    fn parses_setup_mode_and_seeds() {
        const SEED: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
        let cli = Cli::try_parse_from([
            "rad-team", "setup", "nico", "--repo", REPOSITORY, "--seed", SEED,
        ])
        .expect("setup CLI should parse");

        assert_eq!(cli.mode(), Mode::Setup);
        assert_eq!(
            cli.application()
                .expect("application should build")
                .seeds()
                .len(),
            1
        );
    }

    #[test]
    fn identity_only_is_available_only_for_setup() {
        let _cli = Cli::try_parse_from([
            "rad-team",
            "setup",
            "maya",
            "--repo",
            REPOSITORY,
            "--identity-only",
        ])
        .expect("setup should accept identity-only provisioning");
        assert!(matches!(
            Cli::try_parse_from([
                "rad-team",
                "run",
                "maya",
                "--repo",
                REPOSITORY,
                "--identity-only",
            ]),
            Err(error) if error.kind() == ErrorKind::UnknownArgument
        ));
    }

    #[test]
    fn provisions_without_a_repository_but_full_setup_and_run_require_one() {
        let cli = Cli::try_parse_from(["rad-team", "setup", "maya", "--identity-only"])
            .expect("identity provisioning should not require a home or repository");
        assert_eq!(cli.repository(), None);
        let cli = Cli::try_parse_from([
            "rad-team",
            "setup",
            "maya",
            "--identity-only",
            "--home",
            "/team/maya",
        ])
        .expect("identity provisioning should not need a repository");
        assert_eq!(cli.repository(), None);
        assert_eq!(cli.home(), Some(std::path::Path::new("/team/maya")));
        for command in ["setup", "run"] {
            assert!(matches!(
                Cli::try_parse_from(["rad-team", command, "maya", "--home", "/team/maya"]),
                Err(error) if error.kind() == ErrorKind::MissingRequiredArgument
            ));
        }
    }
}
