//! Argument parsing and command dispatch.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use radicle::{
    identity::{Did, RepoId},
    node::NodeId,
};
use tracing::level_filters::LevelFilter;

use crate::{
    Application,
    cli::{
        inspection, provider as provider_command,
        setup::{Error as SetupError, SetupWizard},
    },
    home::{ENVIRONMENT_VARIABLE, MemberHome},
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
    /// Log level: error, warn, info, debug, trace, or off; overrides RUST_LOG.
    #[arg(long, global = true, value_name = "LEVEL")]
    log: Option<LevelFilter>,
}

impl Cli {
    /// Parses the command-line arguments.
    #[must_use]
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Returns the explicit log level, overriding the environment when present.
    #[must_use]
    pub const fn log_level(&self) -> Option<LevelFilter> {
        self.log
    }

    /// Returns the requested operation.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        match self.command {
            Command::Setup(_) => Mode::Setup,
            Command::Run(_) => Mode::Run,
            Command::Provider(_) => Mode::Provider,
            Command::Status(_) => Mode::Status,
            Command::Task(_) => Mode::Task,
        }
    }

    /// Returns the member explicitly selected for setup; `run` reads it from the member home.
    #[must_use]
    pub fn member(&self) -> Option<Member> {
        match &self.command {
            Command::Setup(options) => Some(options.member),
            Command::Run(_) | Command::Provider(_) | Command::Status(_) | Command::Task(_) => None,
        }
    }

    /// Returns the repository explicitly selected for setup; `run` reads it from the member home.
    #[must_use]
    pub fn repository(&self) -> Option<RepoId> {
        match &self.command {
            Command::Setup(options) => options.repo,
            Command::Run(_) | Command::Provider(_) | Command::Status(_) | Command::Task(_) => None,
        }
    }

    /// Returns the explicit application home, if configured.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        match &self.command {
            Command::Provider(options) => options.home.as_deref(),
            Command::Status(options) => options.home.as_deref(),
            Command::Task(options) => options.location.home.as_deref(),
            _ => self.options()?.home.as_deref(),
        }
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
    /// Returns an error when the member directory cannot be resolved, its recorded member or
    /// repository is invalid, or no repository has been selected.
    pub fn application(self) -> Result<Application, Error> {
        match self.command {
            Command::Setup(options) => options.options.application(
                options.member,
                options.repo.ok_or(Error::RepositoryRequired)?,
            ),
            Command::Run(mut options) => {
                let home = match &options.home {
                    Some(path) => MemberHome::at(path),
                    None => MemberHome::discover()?,
                };
                let marker = home.marker()?;
                let member = marker.member()?;
                let repository = marker
                    .repository()?
                    .ok_or_else(|| Error::UnboundIdentity(home.path().into()))?;
                options.home = Some(home.path().into());
                options.application(member, repository)
            }
            Command::Provider(_) | Command::Status(_) | Command::Task(_) => {
                Err(Error::NotAMemberCommand)
            }
        }
    }

    /// Executes the selected command.
    ///
    /// # Errors
    ///
    /// Returns an error when setup or member execution fails.
    pub async fn execute(self) -> Result<(), Error> {
        match self.command {
            Command::Status(options) => {
                let view = inspection::View(crate::status::Inspection::read(options.home).await?);
                println!("{}", view.status());
                Ok(())
            }
            Command::Task(options) => {
                let view =
                    inspection::View(crate::status::Inspection::read(options.location.home).await?);
                match options.command {
                    TaskCommand::Ls => {
                        println!("{}", view.tasks());
                        Ok(())
                    }
                    TaskCommand::Show { id } => {
                        println!("{}", view.task(id)?);
                        Ok(())
                    }
                }
            }
            Command::Provider(options) => {
                let context = provider_command::Context::load(options.home)?;
                match options.command {
                    None => {
                        println!("{}", context.provider()?);
                        Ok(())
                    }
                    Some(ProviderCommand::Ls) => context.list().await,
                    Some(ProviderCommand::Login) => context.login().await,
                    Some(ProviderCommand::Select) => context.select().await,
                }
                .map_err(Error::from)
            }
            command @ (Command::Setup(_) | Command::Run(_)) => {
                Box::pin(
                    Self {
                        command,
                        log: self.log,
                    }
                    .run_member(),
                )
                .await
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
            let member = options.member;
            let home = match &options.options.home {
                Some(home) => crate::home::MemberHome::at(home),
                None => Application::default_setup_home(member, options.repo)?,
            };
            let did = Application::provision_identity(
                member,
                options.repo,
                home,
                options.options.identity_provider(),
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
    /// Reading local runtime status or tasks failed.
    #[error(transparent)]
    Inspection(#[from] crate::status::Error),
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
    #[error("--repo is required for full setup")]
    RepositoryRequired,
    /// The member directory records an unknown member.
    #[error("invalid member in the member directory: {0}")]
    Member(#[from] crate::member::UnknownMember),
    /// The member directory records an invalid repository identifier.
    #[error("invalid repository ID in the member directory: {0}")]
    Repository(#[from] radicle::identity::IdError),
    /// Only an identity has been prepared in this member directory.
    #[error(
        "member home '{0}' has no repository binding; complete setup with --repo and --home first"
    )]
    UnboundIdentity(PathBuf),
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
    /// List, authorize, or select this member's model provider.
    Provider,
    /// Inspect the local runtime and its concurrent roles.
    Status,
    /// Inspect unfinished tasks and their saved context.
    Task,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Prepare a member identity and repository.
    Setup(SetupOptions),
    /// Run the member configured in the current directory or --home.
    Run(Options),
    /// Show the configured provider or manage this member's providers.
    Provider(ProviderOptions),
    /// Show this member's runtime, role activity, and queue length.
    Status(Location),
    /// Inspect this member's unfinished tasks.
    Task(TaskOptions),
}

impl Command {
    const fn options(&self) -> Option<&Options> {
        match self {
            Self::Setup(options) => Some(&options.options),
            Self::Run(options) => Some(options),
            Self::Provider(_) | Self::Status(_) | Self::Task(_) => None,
        }
    }
}

#[derive(Debug, Args)]
struct Location {
    /// Member directory; otherwise locate it from the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct TaskOptions {
    #[command(flatten)]
    location: Location,
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List implementation tasks in execution order.
    Ls,
    /// Show an unfinished task's specification and related artifacts.
    Show { id: i64 },
}

#[derive(Debug, Args)]
struct SetupOptions {
    // Team member to select: nico, maya, leon, vera, nora, or theo.
    member: Member,
    #[command(flatten)]
    options: Options,
    /// Repository to prepare; optional when only provisioning an identity.
    #[arg(short, long, required_unless_present = "identity_only")]
    repo: Option<RepoId>,
    /// Save the member identity and print its DID without starting a node or fetching.
    #[arg(long)]
    identity_only: bool,
}

#[derive(Debug, Args)]
struct ProviderOptions {
    #[command(subcommand)]
    command: Option<ProviderCommand>,
    /// Member directory; defaults to the current member home.
    #[arg(long, global = true, env = ENVIRONMENT_VARIABLE, value_name = "PATH")]
    home: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    /// List available providers and this member's authorization status.
    Ls,
    /// Authorize the provider configured for this member.
    Login,
    /// Choose and authorize a provider, then propose its manifest change.
    Select,
}

#[derive(Debug, Args)]
struct Options {
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
    fn application(self, member: Member, repository: RepoId) -> Result<Application, Error> {
        let mut application =
            Application::new(member, repository).with_identity_provider(self.identity_provider());
        let home = match self.home {
            Some(home) => home,
            None => Application::default_setup_home(member, Some(repository))?
                .path()
                .into(),
        };
        application = application.with_home(home);
        if let Some(manifest) = self.manifest {
            application = application.with_manifest_file(manifest);
        }
        if let Some(provider) = self.provider {
            application = application.with_provider(provider);
        }
        for seed in self.seeds {
            application = application.with_seed(seed);
        }

        Ok(application)
    }

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
    use tracing::level_filters::LevelFilter;

    use crate::{
        cli::{Cli, Error, Mode},
        home::{self, MemberHome},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[test]
    fn log_level_is_optional_global_and_validated() {
        assert_eq!(
            Cli::try_parse_from(["rad-team", "run"])
                .expect("default logging")
                .log_level(),
            None
        );
        for arguments in [
            vec!["rad-team", "--log", "info", "run"],
            vec!["rad-team", "run", "--log", "info"],
            vec!["rad-team", "provider", "ls", "--log", "info"],
        ] {
            assert_eq!(
                Cli::try_parse_from(arguments)
                    .expect("global log level")
                    .log_level(),
                Some(LevelFilter::INFO)
            );
        }
        let error = Cli::try_parse_from(["rad-team", "run", "--log", "loud"])
            .expect_err("unknown levels must be rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn provider_commands_are_member_local_and_take_no_provider_argument() {
        let cli = Cli::try_parse_from(["rad-team", "provider"])
            .expect("provider without a subcommand should parse");
        assert_eq!(cli.mode(), Mode::Provider);
        let cli = Cli::try_parse_from(["rad-team", "provider", "--home", "/team/maya"])
            .expect("current provider accepts an explicit member home");
        assert_eq!(cli.home(), Some(std::path::Path::new("/team/maya")));
        for action in ["ls", "login", "select"] {
            let cli = Cli::try_parse_from(["rad-team", "provider", action, "--home", "/team/maya"])
                .expect("member-local provider command should parse");
            assert_eq!(cli.mode(), Mode::Provider);
            assert_eq!(cli.home(), Some(std::path::Path::new("/team/maya")));
            Cli::try_parse_from(["rad-team", "provider", action, "gpt"])
                .expect_err("the manifest selects the provider");
        }
        Cli::try_parse_from(["rad-team", "providers"]).expect_err("use provider ls");
        Cli::try_parse_from(["rad-team", "login", "gpt"]).expect_err("use provider login");
    }

    #[test]
    fn parses_every_member() {
        for member in Member::ALL {
            assert!(matches!(
                Cli::try_parse_from(["rad-team", "setup", member.id(), "--repo", REPOSITORY]),
                Ok(cli) if cli.member() == Some(member)
            ));
        }
    }

    #[test]
    fn parses_repository() {
        assert!(matches!(
            Cli::try_parse_from(["rad-team", "setup", "nico", "--repo", REPOSITORY]),
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
            Cli::try_parse_from(["rad-team", "run", "--home", "/team"]),
            Ok(cli) if cli.home() == Some(std::path::Path::new("/team"))
        ));
    }

    #[test]
    fn configures_an_existing_identity() {
        const DID: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
        let cli = Cli::try_parse_from([
            "rad-team",
            "run",
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
        let cli = Cli::try_parse_from(["rad-team", "run", "--unencrypted-identity"])
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
                "--expected-did",
                "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            ]),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument
        ));
    }

    #[test]
    fn builds_the_application_from_the_member_home() {
        const SEED: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(root.path());
        let repository = REPOSITORY.parse().expect("repository should parse");
        home.record(Member::Maya, repository)
            .expect("member marker should be recorded");
        let application = Cli::try_parse_from([
            "rad-team",
            "run",
            "--home",
            home.path().to_str().expect("home should be UTF-8"),
            "--manifest",
            "/manifest.toml",
            "--provider",
            "claude",
            "--seed",
            SEED,
            "--unencrypted-identity",
        ])
        .expect("CLI should parse")
        .application();

        let application = application.expect("application should build");
        assert_eq!(application.member(), Member::Maya);
        assert_eq!(application.repository(), repository);
        assert_eq!(application.home(), Some(home.path()));
        assert_eq!(
            application.manifest_file(),
            Some(std::path::Path::new("/manifest.toml"))
        );
        assert_eq!(application.provider(), Some("claude"));
        assert_eq!(
            application.seeds(),
            &[SEED.parse().expect("seed should parse")]
        );
        assert_eq!(
            application.identity_provider(),
            &crate::identity::Provider::Managed {
                protection: crate::identity::KeyProtection::Unencrypted,
            }
        );
        assert!(
            !home.identity().exists(),
            "resolving a run must not provision an identity"
        );
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert!(matches!(
            Cli::try_parse_from(["rad-team", "setup", "unknown", "--repo", REPOSITORY]),
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
        let application = cli.application().expect("application should build");
        assert_eq!(application.seeds().len(), 1);
        assert!(
            application
                .home()
                .expect("home should be selected")
                .ends_with("nico-z3cyotNH")
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
            Cli::try_parse_from(["rad-team", "run", "--identity-only"]),
            Err(error) if error.kind() == ErrorKind::UnknownArgument
        ));
    }

    #[test]
    fn provisions_without_a_repository_but_full_setup_requires_one() {
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
        assert!(matches!(cli.application(), Err(Error::RepositoryRequired)));
        assert!(matches!(
            Cli::try_parse_from(["rad-team", "setup", "maya", "--home", "/team/maya"]),
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument
        ));
    }

    #[test]
    fn run_takes_neither_a_member_nor_a_repository() {
        let cli = Cli::try_parse_from(["rad-team", "run"])
            .expect("run should need no member or repository argument");
        assert_eq!(cli.mode(), Mode::Run);
        assert_eq!(cli.member(), None);
        assert_eq!(cli.repository(), None);
        for arguments in [
            vec!["rad-team", "run", "maya"],
            vec!["rad-team", "run", "--repo", REPOSITORY],
        ] {
            assert!(matches!(
                Cli::try_parse_from(arguments),
                Err(error) if error.kind() == ErrorKind::UnknownArgument
            ));
        }
    }

    #[test]
    fn run_rejects_a_home_without_a_repository_binding() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(root.path());
        home.record_identity(Member::Maya)
            .expect("member marker should be recorded");
        let cli = Cli::try_parse_from([
            "rad-team",
            "run",
            "--home",
            home.path().to_str().expect("home should be UTF-8"),
        ])
        .expect("run should parse");
        assert!(
            matches!(cli.application(), Err(Error::UnboundIdentity(path)) if path == home.path())
        );
        assert!(
            !home.process_lock().exists(),
            "run must fail before acquiring the identity"
        );
    }

    #[test]
    fn run_rejects_missing_or_invalid_member_markers() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(root.path().join("missing"));
        let cli = Cli::try_parse_from([
            "rad-team",
            "run",
            "--home",
            home.path().to_str().expect("home should be UTF-8"),
        ])
        .expect("run should parse");
        assert!(matches!(
            cli.application(),
            Err(Error::Home(home::Error::MarkerUnreadable { .. }))
        ));
        assert!(!home.path().exists(), "run must not create a member home");

        for (member, repository) in [("unknown", REPOSITORY), ("maya", "invalid")] {
            std::fs::write(
                root.path().join(home::MARKER),
                format!("member = {member:?}\nrepository = {repository:?}\n"),
            )
            .expect("invalid marker should be written");
            let cli = Cli::try_parse_from([
                "rad-team",
                "run",
                "--home",
                root.path().to_str().expect("home should be UTF-8"),
            ])
            .expect("run should parse");
            let error = cli
                .application()
                .expect_err("invalid recorded binding must be rejected");
            assert!(matches!(
                (member, error),
                ("unknown", Error::Member(_)) | ("maya", Error::Repository(_))
            ));
        }
    }
}
