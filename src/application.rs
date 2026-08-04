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
    profile::Profile as RadicleProfile,
    storage::{ReadRepository as _, ReadStorage as _},
};

use crate::{
    agent::{
        self, Change, ChatContext, ChatMessage, Coder, Comment, Context as AgentContext, Talker,
        Target,
    },
    builder, chat, conversation,
    home::{self, MemberHome},
    identity::{self, GenerationStore, Manager, MemberLock, Provider},
    manifest::{self, Manifest},
    mcp,
    member::Member,
    node::{self, NodeManager},
    queue::{Job, WorkQueue},
    sandbox::AgentSandbox,
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
        let (status, security) = self.setup_status(runtime).await?;
        let did = runtime.identity.did();

        Ok(Setup {
            member: self.member,
            repository: self.repository,
            did,
            status,
            security,
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
        Box::pin(self.run_with_agents(&mut LoggingAgent, &mut LoggingAgent)).await
    }

    /// Runs the team member with `talker` answering messages and `coder` carrying out queued work.
    ///
    /// The two adapters share one identity, one manifest binding, and one node, but not one train
    /// of thought: a half-written patch has no safe point to pause at, so a question cannot wait
    /// behind it.
    ///
    /// Linux executables must call
    /// [`sandbox::dispatch_linux_helper`](crate::sandbox::dispatch_linux_helper) as the first
    /// statement in `main`.
    ///
    /// # Errors
    ///
    /// Returns an error if preparation, watching, building, or agent dispatch fails.
    pub async fn run_with_agents(
        &self,
        talker: &mut (impl Talker + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let (runtime, binding) = self.start_run().await?;
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            generation = binding.generation(),
            "team member initialized"
        );
        AgentRuntime::start(
            self,
            runtime,
            talker,
            coder,
            binding.mcp_allow().clone(),
            binding.policy().clone(),
        )
        .await?
        .run(self, talker, coder)
        .await
    }

    fn warn_agent_stop(stopped: Result<(), agent::Error>) {
        if let Err(error) = stopped {
            tracing::warn!(%error, "failed to stop agent after application error");
        }
    }

    fn warn_mcp_stop(stopped: Result<(), mcp::Error>) {
        if let Err(error) = stopped {
            tracing::warn!(%error, "failed to stop MCP server after application error");
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
        let binding = self
            .validate_binding(&runtime.home, &runtime.identity)
            .await?;
        self.prepare_security_repository(runtime, binding.security())
            .await?;

        Ok(binding)
    }

    fn member_home(&self) -> Result<MemberHome, home::Error> {
        match &self.home {
            Some(root) => Ok(MemberHome::at(root, self.member)),
            None => MemberHome::discover(self.member),
        }
    }

    pub(crate) fn sandbox_config_path(&self) -> Result<PathBuf, Error> {
        Ok(self.member_home()?.sandbox_config())
    }

    fn load_identity(&self, home: &MemberHome) -> Result<identity::Identity, Error> {
        Manager::default()
            .load(self.member, home, &self.identity_provider)
            .map_err(Error::from)
    }

    async fn synchronize(&self, identity: &identity::Identity) -> Result<(), Error> {
        self.synchronize_repository(identity, self.repository).await
    }

    async fn synchronize_repository(
        &self,
        identity: &identity::Identity,
        repository: RepoId,
    ) -> Result<(), Error> {
        self.ensure_running()?;
        let output = self
            .sync_command(identity, repository)
            .output()
            .await
            .map_err(Error::SyncSpawn)?;
        self.ensure_running()?;
        self.validate_sync_output(identity, repository, output)
    }

    fn sync_command(
        &self,
        identity: &identity::Identity,
        repository: RepoId,
    ) -> tokio::process::Command {
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
        command.arg(repository.to_string());
        identity.configure_radicle_command(command.as_std_mut());

        command
    }

    fn validate_sync_output(
        &self,
        identity: &identity::Identity,
        repository: RepoId,
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
                repository,
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
        self.seed(identity, node, self.repository).await
    }

    async fn seed(
        &self,
        identity: &identity::Identity,
        node: &Node,
        repository: RepoId,
    ) -> Result<(), Error> {
        let profile = identity.profile().clone();
        let mut node = node.clone();
        tokio::task::spawn_blocking(move || profile.seed(repository, Scope::All, &mut node))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Ok(())
    }

    async fn prepare_security_repository(
        &self,
        runtime: &Runtime,
        policy: &manifest::SecurityPolicy,
    ) -> Result<(), Error> {
        let repository = policy.repository();
        self.seed(&runtime.identity, runtime.node.node(), repository)
            .await?;
        self.synchronize_repository(&runtime.identity, repository)
            .await?;
        let storage = runtime.identity.profile().storage.clone();
        let did = runtime.identity.did();
        tokio::task::spawn_blocking(move || {
            let stored = storage.repository(repository)?;
            Self::validate_private_repository_access(&stored, repository, did)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
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
        runtime: &Runtime,
    ) -> Result<(SetupStatus, Option<manifest::SecurityPolicy>), Error> {
        let validation = self.load_setup_validation(&runtime.identity).await?;
        if let Some(policy) = validation.ready_security() {
            self.prepare_security_repository(runtime, policy).await?;
        }
        validation.record(self, &runtime.home).await
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
        self.validate_profile_binding(home, identity.profile(), identity.did())
            .await
    }

    async fn validate_profile_binding(
        &self,
        home: &MemberHome,
        profile: &RadicleProfile,
        did: Did,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let binding = self.load_profile_binding(profile, did).await?;
        self.record_binding(home, binding.clone()).await?;

        Ok(binding)
    }

    async fn load_profile_binding(
        &self,
        profile: &RadicleProfile,
        did: Did,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let storage = profile.storage.clone();
        let repository_id = self.repository;
        let member = self.member;
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

    fn validate_private_repository_access(
        repository: &radicle::storage::git::Repository,
        repository_id: RepoId,
        did: Did,
    ) -> Result<(), Error> {
        let identity = repository.identity_doc()?;
        if identity.is_public() {
            return Err(Error::PublicSecurityRepository(repository_id));
        }
        if identity.is_visible_to(&did) {
            Ok(())
        } else {
            Err(Error::RepositoryAccess {
                repository: repository_id,
                did,
            })
        }
    }

    async fn validate_security_policy(
        &self,
        profile: &RadicleProfile,
        did: Did,
        policy: &manifest::SecurityPolicy,
    ) -> Result<(), Error> {
        let storage = profile.storage.clone();
        let repository = policy.repository();
        tokio::task::spawn_blocking(move || {
            let stored = storage.repository(repository)?;
            Self::validate_private_repository_access(&stored, repository, did)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct LoggingAgent;

#[async_trait::async_trait]
impl Talker for LoggingAgent {
    async fn start(
        &mut self,
        profile: crate::member::Profile,
        _mcp: &agent::McpEndpoint,
        _sandbox: AgentSandbox,
    ) -> Result<(), agent::Error> {
        tracing::info!(
            member = profile.member.id(),
            intelligence = profile.tuning.intelligence,
            speed = profile.tuning.speed,
            experience = profile.tuning.experience,
            creativity = profile.tuning.creativity,
            caution = profile.tuning.caution,
            "agent adapter started"
        );
        Ok(())
    }

    async fn mentioned(
        &mut self,
        context: AgentContext,
        target: Target,
    ) -> Result<(), agent::Error> {
        tracing::info!(?context, ?target, "mention received");
        Ok(())
    }

    async fn messaged(
        &mut self,
        context: ChatContext,
        message: ChatMessage,
    ) -> Result<(), agent::Error> {
        tracing::info!(?context, ?message, "chat message received");
        Ok(())
    }

    async fn replied(
        &mut self,
        context: AgentContext,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<(), agent::Error> {
        tracing::info!(?context, ?comment, %reply_to, "reply received");
        Ok(())
    }

    async fn changed(&mut self, context: AgentContext, change: Change) -> Result<(), agent::Error> {
        tracing::info!(?context, ?change, "relevant change received");
        Ok(())
    }
}

#[async_trait::async_trait]
impl Coder for LoggingAgent {
    async fn start(
        &mut self,
        profile: crate::member::Profile,
        _mcp: &agent::McpEndpoint,
        _sandbox: AgentSandbox,
    ) -> Result<(), agent::Error> {
        tracing::info!(member = profile.member.id(), "coder adapter started");
        Ok(())
    }

    async fn work(&self, job: Job, workspace: PathBuf) -> Result<(), agent::Error> {
        tracing::info!(?job, workspace = %workspace.display(), "queued work received");
        Ok(())
    }

    async fn interrupt(&self) -> Result<(), agent::Error> {
        tracing::info!("queued work asked to stop");
        Ok(())
    }
}

#[derive(Debug)]
struct Runtime {
    home: MemberHome,
    _lock: MemberLock,
    identity: identity::Identity,
    node: NodeManager,
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
struct AgentRuntime {
    runtime: Runtime,
    servers: Servers,
    queue: WorkQueue,
    policy: manifest::RepositoryPolicy,
}

#[derive(Debug)]
struct Servers {
    talker: mcp::Server,
    coder: mcp::Server,
}

impl Servers {
    async fn stop(self) -> Result<(), mcp::Error> {
        let (talker, coder) = tokio::join!(self.talker.stop(), self.coder.stop());

        talker.and(coder)
    }
}

impl AgentRuntime {
    async fn start(
        application: &Application,
        runtime: Runtime,
        talker: &mut (impl Talker + ?Sized),
        coder: &mut (impl Coder + ?Sized),
        allowlist: manifest::McpAllowlist,
        policy: manifest::RepositoryPolicy,
    ) -> Result<Self, Error> {
        match Self::initialize(application, &runtime, talker, coder, allowlist).await {
            Ok((servers, queue)) => Ok(Self {
                runtime,
                servers,
                queue,
                policy,
            }),
            Err(error) => Application::finish_after_stop(
                Err(error),
                runtime.stop(&application.shutdown).await,
            ),
        }
    }

    async fn initialize(
        application: &Application,
        runtime: &Runtime,
        talker: &mut (impl Talker + ?Sized),
        coder: &mut (impl Coder + ?Sized),
        allowlist: manifest::McpAllowlist,
    ) -> Result<(Servers, WorkQueue), Error> {
        let publisher = Self::initialize_publisher(application, runtime).await?;
        let (queue, chat_state) = Self::initialize_state(application, runtime).await?;
        let servers = Self::start_mcp(
            application,
            runtime,
            publisher,
            allowlist,
            &queue,
            &chat_state,
        )
        .await?;
        if let Err(error) = Self::start_agents(application, runtime, &servers, talker, coder).await
        {
            Application::warn_agent_stop(talker.stop().await);
            Application::warn_agent_stop(coder.stop().await);
            Application::warn_mcp_stop(servers.stop().await);
            return Err(error);
        }

        Ok((servers, queue))
    }

    async fn initialize_state(
        application: &Application,
        runtime: &Runtime,
    ) -> Result<(WorkQueue, chat::State), Error> {
        let queue = WorkQueue::open(
            &runtime.home.state_database(),
            application.repository,
            application.member,
        )
        .await?;
        let chat_state = chat::State::open(&runtime.home.state_database())
            .await
            .map_err(|error| Error::Chat(error.to_string()))?;
        Ok((queue, chat_state))
    }

    async fn start_mcp(
        application: &Application,
        runtime: &Runtime,
        publisher: agent::Publisher,
        allowlist: manifest::McpAllowlist,
        queue: &WorkQueue,
        chat: &chat::State,
    ) -> Result<Servers, Error> {
        let storage = runtime.identity.profile().storage.clone();
        let start = async |role| {
            mcp::Server::start(
                storage.clone(),
                application.repository,
                publisher.clone(),
                allowlist.clone(),
                role,
                queue.clone(),
                chat.clone(),
            )
            .await
            .map_err(|error| Error::Mcp(error.to_string()))
        };
        let talker = start(mcp::Role::Talker).await?;
        match start(mcp::Role::Coder).await {
            Ok(coder) => Ok(Servers { talker, coder }),
            Err(error) => {
                Application::warn_mcp_stop(talker.stop().await);
                Err(error)
            }
        }
    }

    async fn start_agents(
        application: &Application,
        runtime: &Runtime,
        servers: &Servers,
        talker: &mut (impl Talker + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let talker_sandbox = AgentSandbox::load(&runtime.home, servers.talker.endpoint())?;
        let coder_sandbox = AgentSandbox::load(&runtime.home, servers.coder.endpoint())?;
        talker
            .start(
                application.member.profile(),
                servers.talker.endpoint(),
                talker_sandbox,
            )
            .await?;
        coder
            .start(
                application.member.profile(),
                servers.coder.endpoint(),
                coder_sandbox,
            )
            .await?;

        Ok(())
    }

    async fn initialize_publisher(
        application: &Application,
        runtime: &Runtime,
    ) -> Result<agent::Publisher, Error> {
        let authorizer = WriteAuthorizer {
            application: application.clone(),
            home: runtime.home.clone(),
            profile: runtime.identity.profile().clone(),
            did: runtime.identity.did(),
        };
        agent::Publisher::new(
            runtime.identity.profile().clone(),
            application.repository,
            runtime.identity.memory_signer(),
            runtime.node.node().clone(),
            authorizer,
        )
        .with_journal(&runtime.home.state_database())
        .await
        .map_err(|error| Error::Mcp(error.to_string()))
    }

    async fn run(
        self,
        application: &Application,
        talker: &mut (impl Talker + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let profile = self.runtime.identity.profile().clone();
        let workspaces = crate::workspace::Workspaces::new(
            self.runtime.home.workspaces(),
            profile.storage.clone(),
            application.repository,
            *profile.id(),
        );
        let state_database = self.runtime.home.state_database();
        let watcher = watcher::Watcher::new(
            application.member,
            application.repository,
            self.policy.clone(),
        );
        let builder = builder::Builder::new(application.member, application.repository);
        let coder_scope = self.servers.coder.scope();
        let (conversations, dispatcher) = conversation::Dispatcher::new(
            application.member,
            workspaces.clone(),
            self.servers.talker.scope(),
        );
        let ran = {
            let watching = watcher.run(
                &profile,
                self.runtime.node.node(),
                &state_database,
                watcher::DeliveryRuntime::new(
                    profile.storage.clone(),
                    self.queue.clone(),
                    self.policy.priority().clone(),
                    conversations.clone(),
                ),
            );
            let dispatching = dispatcher.run(talker);
            let building = builder.run(&*coder, &self.queue, &workspaces, &coder_scope);
            tokio::select! {
                watched = watching => watched.map_err(Error::from),
                dispatched = dispatching => dispatched
                    .map_err(|error| Error::Conversation(error.to_string())),
                built = building => built.map_err(Error::from),
            }
        };
        let interrupted = application.shutdown.is_requested();
        let cleanup = RunCleanup {
            talker: talker.stop().await,
            coder: coder.stop().await,
            mcp: self.servers.stop().await,
            node: self.runtime.stop(&application.shutdown).await,
        };
        cleanup.finish(ran, interrupted)
    }
}

#[derive(Debug, Clone)]
struct WriteAuthorizer {
    application: Application,
    home: MemberHome,
    profile: RadicleProfile,
    did: Did,
}

#[async_trait::async_trait]
impl agent::Authorizer for WriteAuthorizer {
    async fn authorize(&self, method: &str) -> Result<(), agent::PublishError> {
        self.application
            .validate_profile_binding(&self.home, &self.profile, self.did)
            .await
            .and_then(|binding| {
                binding
                    .mcp_allow()
                    .allows(method)
                    .then_some(())
                    .ok_or_else(|| {
                        Error::Mcp(format!("MCP method '{method}' is no longer allowed"))
                    })
            })
            .map_err(|error| agent::PublishError::Authorization(error.to_string()))
    }

    async fn security_policy(&self) -> Result<manifest::SecurityPolicy, agent::PublishError> {
        let binding = self
            .application
            .validate_profile_binding(&self.home, &self.profile, self.did)
            .await
            .map_err(|error| agent::PublishError::Authorization(error.to_string()))?;
        self.application
            .validate_security_policy(&self.profile, self.did, binding.security())
            .await
            .map_err(|error| agent::PublishError::Authorization(error.to_string()))?;

        Ok(binding.security().clone())
    }
}

struct RunCleanup {
    talker: Result<(), agent::Error>,
    coder: Result<(), agent::Error>,
    mcp: Result<(), mcp::Error>,
    node: Result<(), node::Error>,
}

impl RunCleanup {
    fn finish(self, ran: Result<(), Error>, interrupted: bool) -> Result<(), Error> {
        if interrupted {
            return self.interrupted();
        }
        match ran {
            Ok(()) => self.successful(),
            Err(error) => self.run_failed(error),
        }
    }

    fn interrupted(self) -> Result<(), Error> {
        self.warn_agents_stopped();
        Application::warn_mcp_stop(self.mcp);
        self.node.map_err(Error::from)
    }

    fn run_failed(self, error: Error) -> Result<(), Error> {
        self.warn_agents_stopped();
        Application::warn_mcp_stop(self.mcp);
        Application::warn_node_stop(&self.node, "application error");
        Err(error)
    }

    fn warn_agents_stopped(&self) {
        for stopped in [&self.talker, &self.coder] {
            if let Err(error) = stopped {
                tracing::warn!(%error, "failed to stop agent after application error");
            }
        }
    }

    fn successful(self) -> Result<(), Error> {
        let Self {
            talker,
            coder,
            mcp,
            node,
        } = self;
        match talker.and(coder) {
            Ok(()) => Self::after_agents(mcp, node),
            Err(error) => {
                Application::warn_mcp_stop(mcp);
                Application::warn_node_stop(&node, "agent shutdown error");
                Err(error.into())
            }
        }
    }

    fn after_agents(
        mcp: Result<(), mcp::Error>,
        node: Result<(), node::Error>,
    ) -> Result<(), Error> {
        match mcp {
            Ok(()) => node.map_err(Error::from),
            Err(error) => {
                Application::warn_node_stop(&node, "MCP server error");
                Err(Error::Mcp(error.to_string()))
            }
        }
    }
}

#[derive(Debug)]
enum SetupValidation {
    Ready(Box<manifest::ValidatedBinding>),
    ManifestMissing,
    BindingMissing(manifest::SecurityPolicy),
}

impl SetupValidation {
    fn new(manifest: Option<Manifest>, member: Member, did: Did) -> Result<Self, manifest::Error> {
        let Some(manifest) = manifest else {
            return Ok(Self::ManifestMissing);
        };
        match manifest.validate_member(member, did) {
            Ok(binding) => Ok(Self::Ready(Box::new(binding))),
            Err(manifest::Error::Missing { .. }) => {
                Ok(Self::BindingMissing(manifest.security().clone()))
            }
            Err(error) => Err(error),
        }
    }

    const fn ready_security(&self) -> Option<&manifest::SecurityPolicy> {
        match self {
            Self::Ready(binding) => Some(binding.security()),
            Self::ManifestMissing | Self::BindingMissing(_) => None,
        }
    }

    async fn record(
        self,
        application: &Application,
        home: &MemberHome,
    ) -> Result<(SetupStatus, Option<manifest::SecurityPolicy>), Error> {
        match self {
            Self::Ready(binding) => {
                let generation = binding.generation();
                let security = binding.security().clone();
                application.record_binding(home, *binding).await?;
                Ok((SetupStatus::Ready { generation }, Some(security)))
            }
            Self::ManifestMissing => Ok((
                SetupStatus::ManifestMissing {
                    generation: application.next_binding_generation(home).await?,
                },
                None,
            )),
            Self::BindingMissing(security) => Ok((
                SetupStatus::BindingMissing {
                    generation: application.next_binding_generation(home).await?,
                },
                Some(security),
            )),
        }
    }
}

/// Result of preparing one member and repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    member: Member,
    repository: RepoId,
    did: Did,
    status: SetupStatus,
    security: Option<manifest::SecurityPolicy>,
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

    /// Returns existing security policy, if the manifest already exists.
    #[must_use]
    pub const fn security(&self) -> Option<&manifest::SecurityPolicy> {
        self.security.as_ref()
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
                "run interactive setup to provision the private security repository, then bind {} \
                 to {} with generation {generation}",
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
    /// Security reports must never be written to a public repository.
    #[error("security-report repository {0} is public")]
    PublicSecurityRepository(RepoId),
    /// Loading or validating the repository manifest failed.
    #[error("repository manifest error: {0}")]
    Manifest(#[from] manifest::Error),
    /// Recording the binding generation failed.
    #[error("member binding generation error: {0}")]
    Generation(#[from] identity::GenerationError),
    /// Watching Radicle activity failed.
    #[error("Radicle watcher error: {0}")]
    Watcher(#[from] watcher::Error),
    /// Carrying out queued work failed.
    #[error("member builder error: {0}")]
    Builder(#[from] builder::Error),
    /// The conversation dispatcher stopped.
    #[error("conversation dispatcher error: {0}")]
    Conversation(String),
    /// Preparing durable chat state failed.
    #[error("chat state error: {0}")]
    Chat(String),
    /// Opening the member work queue failed.
    #[error("work queue error: {0}")]
    Queue(#[from] crate::queue::Error),
    /// Starting or stopping the run-scoped MCP server failed.
    #[error("MCP server error: {0}")]
    Mcp(String),
    /// Loading the agent sandbox failed.
    #[error("agent sandbox error: {0}")]
    Sandbox(#[from] crate::sandbox::Error),
    /// Starting or stopping the agent adapter failed.
    #[error(transparent)]
    Talker(#[from] agent::Error),
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
    use radicle::{
        crypto::Seed,
        git::Oid,
        identity::doc::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };
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
    #[cfg(not(miri))]
    fn rejects_a_public_security_repository() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = Home::new(root.path().join("home")).expect("Radicle home should be created");
        let profile = Profile::init(home, Alias::new("security-test"), None, Seed::generate())
            .expect("Radicle profile should initialize");
        let working = root.path().join("working");
        let checkout = git2::Repository::init(&working).expect("Git repository should initialize");
        checkout
            .set_head("refs/heads/main")
            .expect("main should become HEAD");
        let tree = {
            let oid = checkout
                .treebuilder(None)
                .expect("tree builder should initialize")
                .write()
                .expect("tree should be written");
            checkout.find_tree(oid).expect("tree should be found")
        };
        let signature = git2::Signature::now("Security Test", "security@example.com")
            .expect("signature should be valid");
        checkout
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Initial",
                &tree,
                &[],
            )
            .expect("commit should be created");
        let signer = profile.signer().expect("signer should load");
        let (repository, _document, _refs) = radicle::rad::init(
            &checkout,
            "security-test"
                .try_into()
                .expect("project name should be valid"),
            "Security test",
            "main".try_into().expect("branch should be valid"),
            Visibility::Public,
            &signer,
            &profile.storage,
        )
        .expect("Radicle repository should initialize");
        let stored = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");

        assert!(matches!(
            Application::validate_private_repository_access(
                &stored,
                repository,
                profile.did(),
            ),
            Err(super::Error::PublicSecurityRepository(actual)) if actual == repository
        ));
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
            security: None,
        };
        let output = setup.to_string();

        assert!(output.contains("nico identity: did:key:"));
        assert!(output.contains("run interactive setup"));
        assert!(output.contains("generation 1"));
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
            crate::manifest::McpAllowlist::default(),
            crate::manifest::SecurityPolicy::test(repository),
            crate::manifest::RepositoryPolicy::new(
                crate::manifest::TriggerAccess::Delegates,
                vec![did],
                crate::manifest::PriorityLabels::default(),
                crate::manifest::Limits::default(),
            ),
        );
        application
            .record_binding(&home, binding.clone())
            .await
            .expect("maximum generation should be recorded");

        let (status, security) = SetupValidation::Ready(Box::new(binding))
            .record(&application, &home)
            .await
            .expect("a ready maximum generation should not be incremented");

        assert_eq!(
            status,
            SetupStatus::Ready {
                generation: i64::MAX.unsigned_abs()
            }
        );
        assert!(security.is_some());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }
}
