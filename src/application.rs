//! Application orchestration for one Radicle team member.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use radicle::{
    cob::EntryId,
    identity::{Did, RepoId},
    node::{Node, NodeId, policy::Scope},
    storage::{ReadRepository as _, ReadStorage as _},
};

use crate::{
    agent::{self, Agent, Change, Comment, Context as AgentContext, Outcome, Target},
    context,
    home::{self, MemberHome},
    identity::{self, GenerationStore, Manager, MemberLock, Provider},
    manifest::{self, Manifest},
    member::Member,
    node::{self, NodeManager},
    shutdown::Shutdown,
    watcher,
};

/// A single Radicle team member application.
#[derive(Debug, Clone)]
pub struct Application {
    member: Member,
    repository: RepoId,
    home: Option<PathBuf>,
    identity_provider: Provider,
    manifest_file: Option<PathBuf>,
    seeds: Vec<NodeId>,
    shutdown: Shutdown,
}

impl Application {
    /// Creates an application for `member` watching `repository`.
    #[must_use]
    pub fn new(member: Member, repository: RepoId) -> Self {
        Self {
            member,
            repository,
            home: None,
            identity_provider: Provider::Managed {
                protection: identity::KeyProtection::SystemCredential,
            },
            manifest_file: None,
            seeds: Vec::new(),
            shutdown: Shutdown::default(),
        }
    }

    pub(crate) fn with_shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Uses an explicit application home directory.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Uses an explicit identity provider.
    #[must_use]
    pub fn with_identity_provider(mut self, provider: Provider) -> Self {
        self.identity_provider = provider;
        self
    }

    /// Uses an explicit manifest file instead of the repository copy.
    #[must_use]
    pub fn with_manifest_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.manifest_file = Some(path.into());
        self
    }

    /// Adds a preferred seed for repository synchronization.
    #[must_use]
    pub fn with_seed(mut self, seed: NodeId) -> Self {
        self.seeds.push(seed);
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

    /// Returns the configured identity provider.
    #[must_use]
    pub const fn identity_provider(&self) -> &Provider {
        &self.identity_provider
    }

    /// Returns the explicit manifest file, if configured.
    #[must_use]
    pub fn manifest_file(&self) -> Option<&Path> {
        self.manifest_file.as_deref()
    }

    /// Returns the preferred synchronization seeds.
    #[must_use]
    pub fn seeds(&self) -> &[NodeId] {
        &self.seeds
    }

    /// Prepares the selected member to work in the repository.
    ///
    /// This provisions the identity, starts its node, synchronizes the repository, and validates
    /// the repository manifest when present.
    ///
    /// # Errors
    ///
    /// Returns an error when identity provisioning, node startup, synchronization, repository
    /// access, or manifest validation fails.
    pub async fn setup(&self) -> Result<Setup, Error> {
        let runtime = Runtime::start(self).await?;
        let setup = self.prepare_setup(&runtime).await;
        let stopped = runtime.stop(&self.shutdown).await;

        Self::finish_after_stop(setup, stopped)
    }

    async fn prepare_setup(&self, runtime: &Runtime) -> Result<Setup, Error> {
        self.synchronize(&runtime.identity).await?;
        let status = self.setup_status(&runtime.home, &runtime.identity).await?;
        let did = runtime.identity.did();

        Ok(Setup {
            member: self.member,
            repository: self.repository,
            did,
            status,
        })
    }

    fn finish_after_stop<T>(
        result: Result<T, Error>,
        stopped: Result<(), node::Error>,
    ) -> Result<T, Error> {
        match result {
            Ok(value) => {
                stopped?;
                Ok(value)
            }
            Err(error) => {
                if let Err(stop) = stopped {
                    tracing::warn!(%stop, "failed to stop node after application error");
                }
                Err(error)
            }
        }
    }

    /// Runs the team member.
    ///
    /// # Errors
    ///
    /// Returns an error if manifest, identity, repository, node, event stream, or local state
    /// validation fails.
    pub async fn run(&self) -> Result<(), Error> {
        self.run_with_agent(&mut LoggingAgent).await
    }

    /// Runs the team member and dispatches relevant activity to `agent`.
    ///
    /// # Errors
    ///
    /// Returns an error if preparation, watching, or agent dispatch fails.
    pub async fn run_with_agent(&self, agent: &mut (impl Agent + ?Sized)) -> Result<(), Error> {
        let (runtime, binding) = self.start_run().await?;
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            generation = binding.generation(),
            "team member initialized"
        );
        AgentRuntime::start(self, runtime, agent)
            .await?
            .run(self, agent)
            .await
    }

    fn warn_agent_stop(stopped: Result<(), agent::Error>) {
        if let Err(error) = stopped {
            tracing::warn!(%error, "failed to stop agent after application error");
        }
    }

    fn warn_context_stop(stopped: Result<(), context::Error>) {
        if let Err(error) = stopped {
            tracing::warn!(%error, "failed to stop context server after application error");
        }
    }

    fn warn_node_stop(stopped: &Result<(), node::Error>, after: &str) {
        if let Err(error) = stopped {
            tracing::warn!(%error, %after, "failed to stop node");
        }
    }

    async fn start_run(&self) -> Result<(Runtime, manifest::ValidatedBinding), Error> {
        let runtime = Runtime::start(self).await?;
        let binding = self.prepare_run(&runtime).await;
        match binding {
            Ok(binding) => Ok((runtime, binding)),
            Err(error) => Self::finish_after_stop(Err(error), runtime.stop(&self.shutdown).await),
        }
    }

    async fn prepare_run(&self, runtime: &Runtime) -> Result<manifest::ValidatedBinding, Error> {
        self.synchronize(&runtime.identity).await?;
        self.validate_binding(&runtime.home, &runtime.identity)
            .await
    }

    fn member_home(&self) -> Result<MemberHome, home::Error> {
        match &self.home {
            Some(root) => Ok(MemberHome::at(root, self.member)),
            None => MemberHome::discover(self.member),
        }
    }

    fn load_identity(&self, home: &MemberHome) -> Result<identity::Identity, Error> {
        Manager::default()
            .load(self.member, home, &self.identity_provider)
            .map_err(Error::from)
    }

    async fn synchronize(&self, identity: &identity::Identity) -> Result<(), Error> {
        self.ensure_running()?;
        let output = self
            .sync_command(identity)
            .output()
            .await
            .map_err(Error::SyncSpawn)?;
        self.ensure_running()?;
        self.validate_sync_output(identity, output)
    }

    fn sync_command(&self, identity: &identity::Identity) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("rad");
        command
            .arg("sync")
            .arg("--fetch")
            .arg("--replicas")
            .arg("1")
            .arg("--timeout")
            .arg("30s");
        for seed in &self.seeds {
            command.arg("--seed").arg(seed.to_string());
        }
        command.arg(self.repository.to_string());
        identity.configure_radicle_command(command.as_std_mut());

        command
    }

    fn validate_sync_output(
        &self,
        identity: &identity::Identity,
        output: std::process::Output,
    ) -> Result<(), Error> {
        if output.status.success() {
            Ok(())
        } else {
            let details = if output.stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            };
            Err(Error::Sync {
                repository: self.repository,
                did: identity.did(),
                status: output.status,
                details,
            })
        }
    }

    async fn seed_repository(
        &self,
        identity: &identity::Identity,
        node: &Node,
    ) -> Result<(), Error> {
        let profile = identity.profile().clone();
        let repository = self.repository;
        let mut node = node.clone();
        tokio::task::spawn_blocking(move || profile.seed(repository, Scope::All, &mut node))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Ok(())
    }

    fn ensure_running(&self) -> Result<(), Error> {
        if self.shutdown.is_requested() {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }

    async fn setup_status(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<SetupStatus, Error> {
        let validation = self.load_setup_validation(identity).await?;
        validation.record(self, home).await
    }

    async fn load_setup_validation(
        &self,
        identity: &identity::Identity,
    ) -> Result<SetupValidation, Error> {
        let storage = identity.profile().storage.clone();
        let repository_id = self.repository;
        let member = self.member;
        let did = identity.did();
        let manifest_file = self.manifest_file.clone();
        tokio::task::spawn_blocking(move || {
            let repository = storage.repository(repository_id)?;
            Self::validate_repository_access(&repository, repository_id, did)?;
            let manifest = match manifest_file {
                Some(path) => Manifest::load_optional_file(&path),
                None => Manifest::load_optional_repository(&repository),
            }?;
            SetupValidation::new(manifest, member, did).map_err(Error::from)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
    }

    async fn next_binding_generation(&self, home: &MemberHome) -> Result<u64, Error> {
        GenerationStore::open(&home.state_database())
            .await?
            .next_generation(self.repository, self.member)
            .await
            .map_err(Error::from)
    }

    async fn validate_binding(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let binding = self.load_validated_binding(identity).await?;
        self.record_binding(home, binding).await?;

        Ok(binding)
    }

    async fn load_validated_binding(
        &self,
        identity: &identity::Identity,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let storage = identity.profile().storage.clone();
        let repository_id = self.repository;
        let member = self.member;
        let did = identity.did();
        let manifest_file = self.manifest_file.clone();
        tokio::task::spawn_blocking(move || {
            let repository = storage.repository(repository_id)?;
            Self::validate_repository_access(&repository, repository_id, did)?;
            let manifest = match manifest_file {
                Some(path) => Manifest::load_file(&path),
                None => Manifest::load_repository(&repository),
            }?;
            manifest.validate_member(member, did).map_err(Error::from)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
    }

    async fn record_binding(
        &self,
        home: &MemberHome,
        binding: manifest::ValidatedBinding,
    ) -> Result<(), Error> {
        GenerationStore::open(&home.state_database())
            .await?
            .record(self.repository, binding)
            .await
            .map_err(Error::from)
    }

    fn validate_repository_access(
        repository: &radicle::storage::git::Repository,
        repository_id: RepoId,
        did: Did,
    ) -> Result<(), Error> {
        if repository.identity_doc()?.is_visible_to(&did) {
            Ok(())
        } else {
            Err(Error::RepositoryAccess {
                repository: repository_id,
                did,
            })
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct LoggingAgent;

#[async_trait::async_trait]
impl Agent for LoggingAgent {
    async fn assigned(&mut self, context: AgentContext) -> Result<Outcome, agent::Error> {
        tracing::info!(?context, "assignment received");
        Ok(Outcome::NoAction)
    }

    async fn mentioned(
        &mut self,
        context: AgentContext,
        target: Target,
    ) -> Result<Outcome, agent::Error> {
        tracing::info!(?context, ?target, "mention received");
        Ok(Outcome::NoAction)
    }

    async fn replied(
        &mut self,
        context: AgentContext,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<Outcome, agent::Error> {
        tracing::info!(?context, ?comment, %reply_to, "reply received");
        Ok(Outcome::NoAction)
    }

    async fn changed(
        &mut self,
        context: AgentContext,
        change: Change,
    ) -> Result<Outcome, agent::Error> {
        tracing::info!(?context, ?change, "relevant change received");
        Ok(Outcome::NoAction)
    }
}

#[derive(Debug)]
struct Runtime {
    home: MemberHome,
    _lock: MemberLock,
    identity: identity::Identity,
    node: NodeManager,
}

#[derive(Debug)]
struct AgentRuntime {
    runtime: Runtime,
    context: context::Server,
}

impl AgentRuntime {
    async fn start(
        application: &Application,
        runtime: Runtime,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<Self, Error> {
        let context = match context::Server::start(
            runtime.identity.profile().storage.clone(),
            application.repository,
        )
        .await
        {
            Ok(context) => context,
            Err(error) => {
                return Application::finish_after_stop(
                    Err(Error::Context(error.to_string())),
                    runtime.stop(&application.shutdown).await,
                );
            }
        };
        if let Err(error) = agent.start(context.endpoint()).await {
            Application::warn_agent_stop(agent.stop().await);
            Application::warn_context_stop(context.stop().await);
            return Application::finish_after_stop(
                Err(error.into()),
                runtime.stop(&application.shutdown).await,
            );
        }
        Ok(Self { runtime, context })
    }

    async fn run(
        self,
        application: &Application,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        let watched = watcher::Watcher::new(application.member, application.repository)
            .run(
                self.runtime.identity.profile(),
                self.runtime.node.node(),
                &self.runtime.home.state_database(),
                agent,
            )
            .await;
        let interrupted = application.shutdown.is_requested();
        let cleanup = RunCleanup {
            agent: agent.stop().await,
            context: self.context.stop().await,
            node: self.runtime.stop(&application.shutdown).await,
        };
        cleanup.finish(watched, interrupted)
    }
}

struct RunCleanup {
    agent: Result<(), agent::Error>,
    context: Result<(), context::Error>,
    node: Result<(), node::Error>,
}

impl RunCleanup {
    fn finish(self, watched: Result<(), watcher::Error>, interrupted: bool) -> Result<(), Error> {
        if interrupted {
            return self.interrupted();
        }
        match watched {
            Ok(()) => self.successful(),
            Err(error) => self.watcher_failed(error),
        }
    }

    fn interrupted(self) -> Result<(), Error> {
        Application::warn_agent_stop(self.agent);
        Application::warn_context_stop(self.context);
        self.node.map_err(Error::from)
    }

    fn watcher_failed(self, error: watcher::Error) -> Result<(), Error> {
        Application::warn_agent_stop(self.agent);
        Application::warn_context_stop(self.context);
        Application::warn_node_stop(&self.node, "application error");
        Err(error.into())
    }

    fn successful(self) -> Result<(), Error> {
        match self.agent {
            Ok(()) => self.after_agent(),
            Err(error) => {
                Application::warn_context_stop(self.context);
                Application::warn_node_stop(&self.node, "agent shutdown error");
                Err(error.into())
            }
        }
    }

    fn after_agent(self) -> Result<(), Error> {
        match self.context {
            Ok(()) => self.node.map_err(Error::from),
            Err(error) => {
                Application::warn_node_stop(&self.node, "context server error");
                Err(Error::Context(error.to_string()))
            }
        }
    }
}

impl Runtime {
    async fn start(application: &Application) -> Result<Self, Error> {
        let home = application.member_home()?;
        let (lock, identity, node) = Self::initialize(application, &home).await?;

        Ok(Self {
            home,
            _lock: lock,
            identity,
            node,
        })
    }

    async fn initialize(
        application: &Application,
        home: &MemberHome,
    ) -> Result<(MemberLock, identity::Identity, NodeManager), Error> {
        let home = home.clone();
        let application = application.clone();
        let worker_home = home.clone();
        let worker_application = application.clone();
        let (lock, identity) = tokio::task::spawn_blocking(move || {
            let lock = MemberLock::acquire(&worker_home)?;
            let identity = worker_application.load_identity(&worker_home)?;
            Ok::<_, Error>((lock, identity))
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        tracing::info!(
            member = %application.member,
            did = %identity.did(),
            "member identity loaded"
        );
        let node = Self::start_node(&application, &home, &identity).await?;

        Ok((lock, identity, node))
    }

    async fn start_node(
        application: &Application,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<NodeManager, Error> {
        let node = NodeManager::start_supervised(identity, home, &application.shutdown).await?;
        match Self::prepare_node(application, identity, &node).await {
            Ok(()) => Ok(node),
            Err(error) => {
                Self::stop_after_start_error(node, &application.shutdown).await;
                Err(error)
            }
        }
    }

    async fn prepare_node(
        application: &Application,
        identity: &identity::Identity,
        node: &NodeManager,
    ) -> Result<(), Error> {
        application.ensure_running()?;
        application.seed_repository(identity, node.node()).await
    }

    async fn stop_after_start_error(node: NodeManager, shutdown: &Shutdown) {
        if let Err(stop) = Self::stop_node(node, shutdown).await {
            tracing::warn!(%stop, "failed to stop node after application error");
        }
    }

    async fn stop(self, shutdown: &Shutdown) -> Result<(), node::Error> {
        Self::stop_node(self.node, shutdown).await
    }

    async fn stop_node(node: NodeManager, shutdown: &Shutdown) -> Result<(), node::Error> {
        let result = node.stop_supervised(shutdown).await;
        shutdown.release_supervision();
        result
    }
}

#[derive(Debug)]
enum SetupValidation {
    Ready(manifest::ValidatedBinding),
    ManifestMissing,
    BindingMissing,
}

impl SetupValidation {
    fn new(manifest: Option<Manifest>, member: Member, did: Did) -> Result<Self, manifest::Error> {
        let Some(manifest) = manifest else {
            return Ok(Self::ManifestMissing);
        };
        match manifest.validate_member(member, did) {
            Ok(binding) => Ok(Self::Ready(binding)),
            Err(manifest::Error::Missing { .. }) => Ok(Self::BindingMissing),
            Err(error) => Err(error),
        }
    }

    async fn record(
        self,
        application: &Application,
        home: &MemberHome,
    ) -> Result<SetupStatus, Error> {
        match self {
            Self::Ready(binding) => {
                application.record_binding(home, binding).await?;
                Ok(SetupStatus::Ready {
                    generation: binding.generation(),
                })
            }
            Self::ManifestMissing => Ok(SetupStatus::ManifestMissing {
                generation: application.next_binding_generation(home).await?,
            }),
            Self::BindingMissing => Ok(SetupStatus::BindingMissing {
                generation: application.next_binding_generation(home).await?,
            }),
        }
    }
}

/// Result of preparing one member and repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    member: Member,
    repository: RepoId,
    did: Did,
    status: SetupStatus,
}

impl Setup {
    /// Returns the provisioned member identity.
    #[must_use]
    pub const fn did(&self) -> Did {
        self.did
    }

    /// Returns the setup status.
    #[must_use]
    pub const fn status(&self) -> SetupStatus {
        self.status
    }
}

impl fmt::Display for Setup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{} identity: {}", self.member, self.did)?;
        match self.status {
            SetupStatus::Ready { generation } => write!(
                formatter,
                "{} is ready for {} with binding generation {generation}",
                self.member, self.repository
            ),
            SetupStatus::ManifestMissing { generation } => write!(
                formatter,
                "add .radicle-team.toml to the repository's canonical branch:\n\nversion = \
                 1\n\n[members.{}]\nsigning_did = \"{}\"\ngeneration = {generation}\nstate = \
                 \"active\"",
                self.member, self.did,
            ),
            SetupStatus::BindingMissing { generation } => write!(
                formatter,
                "add this binding to .radicle-team.toml:\n\n[members.{}]\nsigning_did = \
                 \"{}\"\ngeneration = {generation}\nstate = \"active\"",
                self.member, self.did,
            ),
        }
    }
}

/// Readiness of a member's repository manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStatus {
    /// Identity, repository access, and the manifest are valid.
    Ready {
        /// Validated identity-binding generation.
        generation: u64,
    },
    /// The repository has no `.radicle-team.toml` manifest.
    ManifestMissing {
        /// Generation to use for the new binding.
        generation: u64,
    },
    /// The manifest does not bind the selected member.
    BindingMissing {
        /// Generation to use for the new binding.
        generation: u64,
    },
}

impl SetupStatus {
    /// Returns the validated or suggested binding generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        match self {
            Self::Ready { generation }
            | Self::ManifestMissing { generation }
            | Self::BindingMissing { generation } => generation,
        }
    }
}

/// Error running a team member application.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Resolving the application home failed.
    #[error("failed to resolve team member home: {0}")]
    Home(#[from] home::Error),
    /// Acquiring exclusive member-home ownership failed.
    #[error("failed to acquire member identity: {0}")]
    Lock(#[from] identity::LockError),
    /// Provisioning or loading the signing identity failed.
    #[error("failed to load member identity: {0}")]
    Identity(#[from] identity::ProviderError),
    /// Starting or stopping the managed Radicle node failed.
    #[error("managed Radicle node error: {0}")]
    Node(#[from] node::Error),
    /// Process termination was requested.
    #[error("process termination requested")]
    Interrupted,
    /// Persisting repository seeding policy failed.
    #[error("failed to seed repository: {0}")]
    Seed(#[from] radicle::profile::Error),
    /// The `rad sync` subprocess could not be started.
    #[error("failed to start 'rad sync': {0}")]
    SyncSpawn(io::Error),
    /// Repository synchronization failed.
    #[error("failed to synchronize {repository} as {did} ({status}): {details}")]
    Sync {
        /// Repository being synchronized.
        repository: RepoId,
        /// Member identity used for synchronization.
        did: Did,
        /// Subprocess exit status.
        status: ExitStatus,
        /// Diagnostic output from `rad sync`.
        details: String,
    },
    /// Opening or validating the Radicle repository failed.
    #[error("failed to access Radicle repository: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    /// A private repository does not authorize this identity.
    #[error("private repository {repository} does not authorize identity {did}")]
    RepositoryAccess {
        /// Repository denying access.
        repository: RepoId,
        /// Rejected member identity.
        did: Did,
    },
    /// Loading or validating the repository manifest failed.
    #[error("repository manifest error: {0}")]
    Manifest(#[from] manifest::Error),
    /// Recording the binding generation failed.
    #[error("member binding generation error: {0}")]
    Generation(#[from] identity::GenerationError),
    /// Watching Radicle activity failed.
    #[error("Radicle watcher error: {0}")]
    Watcher(#[from] watcher::Error),
    /// Starting or stopping the run-scoped context server failed.
    #[error("agent context server error: {0}")]
    Context(String),
    /// Starting or stopping the agent adapter failed.
    #[error(transparent)]
    Agent(#[from] agent::Error),
    /// A blocking filesystem or Radicle task failed.
    #[error("application task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {
    #[cfg(not(miri))]
    use std::{fs, path::PathBuf};
    use std::str::FromStr as _;

    #[cfg(not(miri))]
    use radicle::git::Oid;
    use radicle::identity::RepoId;

    #[cfg(not(miri))]
    use crate::{
        application::SetupValidation,
        home::MemberHome,
        manifest::{Revision, ValidatedBinding},
    };
    use crate::{Application, Setup, SetupStatus, member::Member};

    #[test]
    fn application_retains_selection() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let application = Application::new(Member::Vera, repository);

        assert_eq!(application.member(), Member::Vera);
        assert_eq!(application.repository(), repository);
        assert_eq!(application.home(), None);
        assert!(matches!(
            application.identity_provider(),
            crate::identity::Provider::Managed { .. }
        ));
        assert_eq!(application.manifest_file(), None);
        assert!(application.seeds().is_empty());
    }

    #[test]
    fn application_accepts_explicit_home() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let application = Application::new(Member::Vera, repository).with_home("/team");

        assert_eq!(application.home(), Some(std::path::Path::new("/team")));
    }

    #[test]
    fn application_accepts_identity_and_repository_manifest() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let provider = crate::identity::Provider::RadicleHome {
            home: "/radicle".into(),
            expected_did: None,
        };
        let application = Application::new(Member::Vera, repository)
            .with_identity_provider(provider.clone())
            .with_manifest_file("/manifest.toml");

        assert_eq!(application.identity_provider(), &provider);
        assert_eq!(
            application.manifest_file(),
            Some(std::path::Path::new("/manifest.toml"))
        );
    }

    #[test]
    fn setup_explains_a_missing_manifest() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let did = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            .parse()
            .expect("DID should be valid");
        let setup = Setup {
            member: Member::Nico,
            repository,
            did,
            status: SetupStatus::ManifestMissing { generation: 1 },
        };
        let output = setup.to_string();

        assert!(output.contains("nico identity: did:key:"));
        assert!(output.contains("[members.nico]"));
        assert!(output.contains("version = 1"));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn accepts_a_ready_binding_at_the_maximum_generation() {
        let root = PathBuf::from("target/tests/application/max-ready-generation");
        let _result = fs::remove_dir_all(&root);
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let did = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            .parse()
            .expect("DID should be valid");
        let application = Application::new(Member::Nico, repository);
        let home = MemberHome::at(&root, Member::Nico);
        let binding = ValidatedBinding::new(
            Member::Nico,
            did,
            i64::MAX.unsigned_abs(),
            Revision::Repository(Oid::from_sha1([0; 20])),
        );
        application
            .record_binding(&home, binding)
            .await
            .expect("maximum generation should be recorded");

        let status = SetupValidation::Ready(binding)
            .record(&application, &home)
            .await
            .expect("a ready maximum generation should not be incremented");

        assert_eq!(
            status,
            SetupStatus::Ready {
                generation: i64::MAX.unsigned_abs()
            }
        );
        fs::remove_dir_all(root).expect("fixture should be removed");
    }
}
