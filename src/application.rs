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
        self, Change, ChatContext, ChatMessage, Chatter, Coder, Comment, Context as AgentContext,
        Talker, Target,
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
        let runtime = Runtime::start(self, Ownership::Claim).await?;
        let setup = self.prepare_setup(&runtime).await;
        let stopped = runtime.stop(&self.shutdown).await;

        Self::finish_after_stop(setup, stopped)
    }

    /// Takes the member directory the way `ownership` asks for, under the process lock.
    fn take_home(&self, home: &MemberHome, ownership: Ownership) -> Result<(), Error> {
        match ownership {
            Ownership::Claim => self.claim_home(home),
            Ownership::Verify => self.validate_member_home(home),
        }
    }

    /// Creates the member directory, refusing one which already belongs to somebody else.
    ///
    /// Checked before anything is written, and before the identity behind the directory is
    /// loaded. A mistyped `--home` would otherwise stamp this member's marker onto another
    /// member's directory and take its identity with it.
    ///
    /// A directory holding an identity but no marker is refused rather than claimed. The marker
    /// is written before the identity is created, so the pair can only be missing the first if
    /// something removed it, and guessing whose key is in there is exactly the mistake above.
    fn claim_home(&self, home: &MemberHome) -> Result<(), Error> {
        if home.path().join(crate::home::MARKER).is_file() {
            self.validate_member_home(home)?;
        } else if home.holds_identity() {
            return Err(Error::MemberDirectory {
                path: home.path().into(),
                reason: format!(
                    "it holds a Radicle identity but no '{}', so which member owns it cannot be \
                     established",
                    crate::home::MARKER
                ),
            });
        }

        home.record(self.member, self.repository)
            .map_err(Error::from)
    }

    async fn prepare_setup(&self, runtime: &Runtime) -> Result<Setup, Error> {
        self.synchronize(&runtime.identity).await?;
        let (status, security) = self.setup_status(runtime).await?;
        let did = runtime.identity.did();
        let chat_certificate = Self::initialize_chat_identity(&runtime.home, self.member)
            .await?
            .fingerprint();

        Ok(Setup {
            member: self.member,
            repository: self.repository,
            did,
            chat_certificate,
            status,
            security,
        })
    }

    async fn initialize_chat_identity(
        home: &MemberHome,
        member: Member,
    ) -> Result<chat::credentials::Credentials, Error> {
        let path = home.chat_identity();
        tokio::task::spawn_blocking(move || {
            chat::credentials::Credentials::load_or_generate(&path, member.id())
                .map_err(|error| Error::Chat(error.to_string()))
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
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
        Box::pin(self.run_with_agents(&mut LoggingAgent, &mut LoggingAgent, &mut LoggingAgent))
            .await
    }

    /// Runs the team member with `talker` answering repository activity, `chatter` answering
    /// chat, and `coder` carrying out queued work.
    ///
    /// Adapters are internal on purpose. The runtime holds each agent's process so it can stop one
    /// which has gone silent or refused to stop, which it could not do for an implementation it
    /// did not own.
    ///
    /// The three adapters share one identity, one manifest binding, and one node, but not one
    /// train of thought. A half-written patch has no safe point to pause at, and an agent turn
    /// cannot be interrupted, so neither a queued job nor a review may hold up somebody waiting in
    /// a channel.
    ///
    /// Enabling chat therefore costs a third concurrent agent for this member whenever a message
    /// arrives, on top of the talking and building adapters.
    ///
    /// Linux executables must call
    /// [`sandbox::dispatch_linux_helper`](crate::sandbox::dispatch_linux_helper) as the first
    /// statement in `main`.
    ///
    /// # Errors
    ///
    /// Returns an error if preparation, watching, building, or agent dispatch fails.
    pub(crate) async fn run_with_agents(
        &self,
        talker: &mut (impl Talker + ?Sized),
        chatter: &mut (impl Chatter + ?Sized),
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
            chatter,
            coder,
            binding.mcp_allow().clone(),
            binding.policy().clone(),
        )
        .await?
        .run(self, talker, chatter, coder)
        .await
    }

    fn warn_agent_shutdown(stopped: Result<(), agent::Error>) {
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
        let runtime = Runtime::start(self, Ownership::Verify).await?;
        match self.prepare_run(&runtime).await {
            Ok(binding) => Ok((runtime, binding)),
            Err(error) => self.abandon_run(runtime, error).await,
        }
    }

    // Stops a runtime whose preparation failed, keeping the original failure.
    async fn abandon_run<T>(&self, runtime: Runtime, error: Error) -> Result<T, Error> {
        Self::finish_after_stop(Err(error), runtime.stop(&self.shutdown).await)
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

    // Resolves this member's directory.
    fn member_home(&self) -> Result<MemberHome, home::Error> {
        match &self.home {
            // Canonicalised because Git resolves a worktree destination against the object store
            // rather than the working directory, so a relative path would create it elsewhere.
            Some(root) => Ok(MemberHome::at(
                std::fs::canonicalize(root).unwrap_or_else(|_error| root.clone()),
            )),
            None => MemberHome::discover(),
        }
    }

    // Confirms the member directory belongs to the member and repository being run.
    fn validate_member_home(&self, home: &MemberHome) -> Result<(), Error> {
        let marker = home.marker()?;
        let member = marker.member().map_err(|error| Error::MemberDirectory {
            path: home.path().into(),
            reason: error.to_string(),
        })?;
        let repository = marker
            .repository()
            .map_err(|error| Error::MemberDirectory {
                path: home.path().into(),
                reason: error.to_string(),
            })?;
        if member != self.member || repository != self.repository {
            return Err(Error::MemberDirectory {
                path: home.path().into(),
                reason: format!("it belongs to {member} in {repository}"),
            });
        }

        Ok(())
    }

    /// Returns the directory `setup` creates when none was given.
    pub(crate) fn default_setup_home(
        member: Member,
        repository: RepoId,
    ) -> Result<MemberHome, home::Error> {
        let working = std::env::current_dir().map_err(home::Error::WorkingDirectory)?;
        let urn = repository.urn();
        let short: String = urn
            .strip_prefix("rad:")
            .unwrap_or(&urn)
            .chars()
            .take(8)
            .collect();

        Ok(MemberHome::at(
            working.join(format!("{}-{short}", member.id())),
        ))
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

impl crate::agent::Supervised for LoggingAgent {
    fn supervisor(&self) -> std::sync::Arc<dyn crate::agent::Supervisor> {
        std::sync::Arc::new(crate::agent::Idle)
    }
}

#[async_trait::async_trait]
impl Chatter for LoggingAgent {
    async fn start(
        &mut self,
        profile: crate::member::Profile,
        _mcp: &agent::McpEndpoint,
        _sandbox: AgentSandbox,
        _layout: crate::agent::Layout,
    ) -> Result<(), agent::Error> {
        tracing::info!(member = profile.member.id(), "chatter adapter started");
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
}

#[async_trait::async_trait]
impl Talker for LoggingAgent {
    async fn start(
        &mut self,
        profile: crate::member::Profile,
        _mcp: &agent::McpEndpoint,
        _sandbox: AgentSandbox,
        _layout: crate::agent::Layout,
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
        _layout: crate::agent::Layout,
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

/// What a start does with the member directory once it holds the process lock.
#[derive(Debug, Clone, Copy)]
enum Ownership {
    /// `setup` records the marker, taking a directory which is free or already this member's.
    Claim,
    /// A run only confirms the directory is this member's.
    Verify,
}

#[derive(Debug)]
struct Runtime {
    home: MemberHome,
    _lock: MemberLock,
    identity: identity::Identity,
    node: NodeManager,
}

impl Runtime {
    async fn start(application: &Application, ownership: Ownership) -> Result<Self, Error> {
        let home = application.member_home()?;
        let (lock, identity, node) = Self::initialize(application, &home, ownership).await?;

        Ok(Self {
            home,
            _lock: lock,
            identity,
            node,
        })
    }

    // Acquires the process lock, then settles the directory and loads the identity behind it.
    async fn initialize(
        application: &Application,
        home: &MemberHome,
        ownership: Ownership,
    ) -> Result<(MemberLock, identity::Identity, NodeManager), Error> {
        let home = home.clone();
        let application = application.clone();
        let worker_home = home.clone();
        let worker_application = application.clone();
        let (lock, identity) = tokio::task::spawn_blocking(move || {
            let lock = MemberLock::acquire(&worker_home)?;
            worker_application.take_home(&worker_home, ownership)?;
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
    resources: Resources,
    policy: manifest::RepositoryPolicy,
}

/// Everything a run needs beyond its identity and node.
#[derive(Debug)]
struct Resources {
    servers: Servers,
    queue: WorkQueue,
    chat_state: chat::State,
    chat_identity: Option<chat::credentials::Credentials>,
    workspaces: crate::workspace::Workspaces,
}

#[derive(Debug)]
struct Servers {
    talker: mcp::Server,
    // Absent when the manifest has no chat section, so a repository without chat runs two adapters
    // rather than paying for a third which would never be addressed.
    chatter: Option<mcp::Server>,
    coder: mcp::Server,
}

impl Servers {
    /// Stops all three at once, so one draining its connections does not delay the others.
    async fn stop(self) -> Result<(), mcp::Error> {
        let chatter = async {
            match self.chatter {
                Some(server) => server.stop().await,
                None => Ok(()),
            }
        };
        let (talker, chatter, coder) = tokio::join!(self.talker.stop(), chatter, self.coder.stop());

        talker.and(chatter).and(coder)
    }
}

impl AgentRuntime {
    async fn start(
        application: &Application,
        runtime: Runtime,
        talker: &mut (impl Talker + ?Sized),
        chatter: &mut (impl Chatter + ?Sized),
        coder: &mut (impl Coder + ?Sized),
        allowlist: manifest::McpAllowlist,
        policy: manifest::RepositoryPolicy,
    ) -> Result<Self, Error> {
        match Self::initialize(
            application,
            &runtime,
            talker,
            chatter,
            coder,
            allowlist,
            &policy,
        )
        .await
        {
            Ok(resources) => Ok(Self {
                runtime,
                resources,
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
        chatter: &mut (impl Chatter + ?Sized),
        coder: &mut (impl Coder + ?Sized),
        allowlist: manifest::McpAllowlist,
        policy: &manifest::RepositoryPolicy,
    ) -> Result<Resources, Error> {
        let (publisher, queue, chat_state, chat_identity) =
            Self::initialize_resources(application, runtime, policy).await?;
        // Before any agent starts. The sandbox fences off the parts of the shared object store an
        // agent must never write, and a path which is not there yet cannot be fenced.
        let workspaces = Self::initialize_workspaces(application, runtime, &queue).await?;
        let servers = Self::start_mcp(
            application,
            runtime,
            publisher,
            allowlist,
            mcp::ToolState::new(queue.clone(), chat_state.clone()),
            policy.chat().is_some(),
        )
        .await?;
        if let Err(error) =
            Self::start_agents(application, runtime, &servers, talker, chatter, coder).await
        {
            Application::warn_agent_shutdown(talker.shutdown().await);
            Application::warn_agent_shutdown(chatter.shutdown().await);
            Application::warn_agent_shutdown(coder.shutdown().await);
            Application::warn_mcp_stop(servers.stop().await);
            return Err(error);
        }

        Ok(Resources {
            servers,
            queue,
            chat_state,
            chat_identity,
            workspaces,
        })
    }

    // Creates the shared object store and every adapter's working copy.
    async fn initialize_workspaces(
        application: &Application,
        runtime: &Runtime,
        queue: &WorkQueue,
    ) -> Result<crate::workspace::Workspaces, Error> {
        let profile = runtime.identity.profile();
        let workspaces = crate::workspace::Workspaces::new(
            application.member,
            runtime.home.clone(),
            profile.storage.clone(),
            application.repository,
            profile.info(),
            queue.clone(),
        );
        workspaces
            .initialize()
            .await
            .map_err(|error| Error::Workspace(error.to_string()))?;

        Ok(workspaces)
    }

    async fn initialize_resources(
        application: &Application,
        runtime: &Runtime,
        policy: &manifest::RepositoryPolicy,
    ) -> Result<
        (
            agent::Publisher,
            WorkQueue,
            chat::State,
            Option<chat::credentials::Credentials>,
        ),
        Error,
    > {
        let publisher = Self::initialize_publisher(application, runtime).await?;
        let (queue, chat_state) = Self::initialize_state(application, runtime).await?;
        let chat_identity = Self::initialize_chat(application, runtime, policy).await?;
        Ok((publisher, queue, chat_state, chat_identity))
    }

    async fn initialize_chat(
        application: &Application,
        runtime: &Runtime,
        policy: &manifest::RepositoryPolicy,
    ) -> Result<Option<chat::credentials::Credentials>, Error> {
        match policy.chat() {
            Some(_config) => {
                Application::initialize_chat_identity(&runtime.home, application.member)
                    .await
                    .map(Some)
            }
            None => Ok(None),
        }
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
        queue.recover().await?;
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
        state: mcp::ToolState,
        chat: bool,
    ) -> Result<Servers, Error> {
        let storage = runtime.identity.profile().storage.clone();
        let start = async |role| {
            Self::start_server(
                storage.clone(),
                application.repository,
                publisher.clone(),
                allowlist.clone(),
                role,
                state.clone(),
            )
            .await
        };

        Self::assembled(Self::opened(Self::wanted_servers(chat), start).await?)
    }

    /// Starts one MCP server for one adapter.
    async fn start_server(
        storage: radicle::storage::git::Storage,
        repository: RepoId,
        publisher: agent::Publisher,
        allowlist: manifest::McpAllowlist,
        role: mcp::Role,
        state: mcp::ToolState,
    ) -> Result<mcp::Server, Error> {
        mcp::Server::start(storage, repository, publisher, allowlist, role, state)
            .await
            .map_err(|error| Error::Mcp(error.to_string()))
    }

    // Lists the servers to start.
    fn wanted_servers(chat: bool) -> Vec<mcp::Role> {
        match chat {
            true => vec![mcp::Role::Talker, mcp::Role::Chatter, mcp::Role::Coder],
            false => vec![mcp::Role::Talker, mcp::Role::Coder],
        }
    }

    // Starts every wanted server, stopping the ones which did start if a later one fails.
    async fn opened<F>(
        wanted: Vec<mcp::Role>,
        start: impl Fn(mcp::Role) -> F,
    ) -> Result<Vec<(mcp::Role, mcp::Server)>, Error>
    where
        F: Future<Output = Result<mcp::Server, Error>>,
    {
        let mut started = Vec::new();
        for role in wanted {
            match start(role).await {
                Ok(server) => started.push((role, server)),
                Err(error) => {
                    let running = started.into_iter().map(|(_role, server)| server).collect();

                    return Err(Self::abandon(error, running).await);
                }
            }
        }

        Ok(started)
    }

    // Sorts the started servers back into the adapters they belong to.
    fn assembled(mut started: Vec<(mcp::Role, mcp::Server)>) -> Result<Servers, Error> {
        let mut take = |wanted| {
            started
                .iter()
                .position(|(role, _server)| *role == wanted)
                .map(|found| started.remove(found).1)
        };
        let missing = || Error::Mcp("an MCP server did not start".to_owned());

        Ok(Servers {
            talker: take(mcp::Role::Talker).ok_or_else(missing)?,
            chatter: take(mcp::Role::Chatter),
            coder: take(mcp::Role::Coder).ok_or_else(missing)?,
        })
    }

    // Stops whatever was already running after a later server failed to start.
    async fn abandon(error: Error, started: Vec<mcp::Server>) -> Error {
        for server in started {
            Application::warn_mcp_stop(server.stop().await);
        }

        error
    }

    async fn start_agents(
        application: &Application,
        runtime: &Runtime,
        servers: &Servers,
        talker: &mut (impl Talker + ?Sized),
        chatter: &mut (impl Chatter + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let profile = application.member.profile();
        let (sandbox, layout) = Self::prepared(
            application,
            runtime,
            agent::Adapter::Talker,
            &servers.talker,
        )?;
        talker
            .start(profile, servers.talker.endpoint(), sandbox, layout)
            .await?;
        Self::start_chatter(application, runtime, servers.chatter.as_ref(), chatter).await?;
        let (sandbox, layout) =
            Self::prepared(application, runtime, agent::Adapter::Coder, &servers.coder)?;
        coder
            .start(profile, servers.coder.endpoint(), sandbox, layout)
            .await
            .map_err(Error::from)
    }

    // Starts the chatting adapter, if the manifest asked for chat at all.
    async fn start_chatter(
        application: &Application,
        runtime: &Runtime,
        server: Option<&mcp::Server>,
        chatter: &mut (impl Chatter + ?Sized),
    ) -> Result<(), Error> {
        let Some(server) = server else {
            return Ok(());
        };
        let (sandbox, layout) =
            Self::prepared(application, runtime, agent::Adapter::Chatter, server)?;

        chatter
            .start(
                application.member.profile(),
                server.endpoint(),
                sandbox,
                layout,
            )
            .await
            .map_err(Error::from)
    }

    // Prepares one adapter's sandbox and its fixed directories.
    fn prepared(
        application: &Application,
        runtime: &Runtime,
        adapter: agent::Adapter,
        server: &mcp::Server,
    ) -> Result<(AgentSandbox, agent::Layout), Error> {
        let sandbox = AgentSandbox::load(
            &runtime.home,
            application.member,
            adapter,
            server.endpoint(),
        )?;

        Ok((sandbox, Self::layout(&runtime.home, adapter)))
    }

    // Returns the fixed directories one adapter owns, which [`MemberHome::create`] made.
    fn layout(home: &MemberHome, adapter: agent::Adapter) -> agent::Layout {
        agent::Layout::new(adapter, home.cache(), home.harness(adapter))
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
        chatter: &mut (impl Chatter + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let profile = self.runtime.identity.profile().clone();
        let workspaces = self.resources.workspaces.clone();
        let state_database = self.runtime.home.state_database();
        let watcher = watcher::Watcher::new(
            application.member,
            application.repository,
            self.policy.clone(),
        );
        let builder = builder::Builder::new(application.member, application.repository);
        let coder_scope = self.resources.servers.coder.scope();
        let (conversations, dispatcher) = conversation::Dispatcher::new(
            application.member,
            workspaces.clone(),
            self.resources.servers.talker.scope(),
        );
        let (messages, chat_dispatcher) = chat::dispatcher::Dispatcher::new(
            application.member,
            workspaces.clone(),
            self.resources
                .servers
                .chatter
                .as_ref()
                .map(mcp::Server::scope)
                .unwrap_or_default(),
        );
        // Kept alive for the whole run. Dropped, its channel would close, the chat dispatcher
        // would return at once, and a member with no chat would exit the moment it started.
        let chat = self.chat_ingress(application, messages.clone());
        let watching = watcher.run(
            &profile,
            self.runtime.node.node(),
            &state_database,
            watcher::DeliveryRuntime::new(
                profile.storage.clone(),
                self.resources.queue.clone(),
                self.policy.priority().clone(),
                conversations.clone(),
            ),
        );
        let dispatching = dispatcher.run(talker);
        let messaging = chat_dispatcher.run(chatter);
        let building = builder.run(&*coder, &self.resources.queue, &workspaces, &coder_scope);
        let chatting = async move {
            if let Some(ingress) = chat
                && let Err(error) = ingress.run().await
            {
                tracing::error!(%error, "chat ingress stopped; repository work remains active");
            }
            std::future::pending::<std::convert::Infallible>().await
        };
        let ran = tokio::select! {
            watched = watching => watched.map_err(Error::from),
            dispatched = dispatching => dispatched
                .map_err(|error| Error::Conversation(error.to_string())),
            messaged = messaging => messaged
                .map_err(|error| Error::Conversation(error.to_string())),
            built = building => built.map_err(Error::from),
            never = chatting => match never {},
        };
        let interrupted = application.shutdown.is_requested();
        let cleanup = RunCleanup {
            talker: talker.shutdown().await,
            chatter: chatter.shutdown().await,
            coder: coder.shutdown().await,
            mcp: self.resources.servers.stop().await,
            node: self.runtime.stop(&application.shutdown).await,
        };
        cleanup.finish(ran, interrupted)
    }

    fn chat_ingress(
        &self,
        application: &Application,
        messages: chat::dispatcher::Handle,
    ) -> Option<chat::runtime::Ingress> {
        let credentials = self.resources.chat_identity.as_ref()?;
        let storage = self.runtime.identity.profile().storage.clone();

        Some(chat::runtime::Ingress::new(
            application.repository,
            application.member,
            self.runtime.identity.did(),
            self.policy.clone(),
            storage,
            credentials.identity(),
            self.resources.chat_state.clone(),
            messages,
        ))
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
    chatter: Result<(), agent::Error>,
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
        for stopped in [&self.talker, &self.chatter, &self.coder] {
            if let Err(error) = stopped {
                tracing::warn!(%error, "failed to stop agent after application error");
            }
        }
    }

    fn successful(self) -> Result<(), Error> {
        let Self {
            talker,
            chatter,
            coder,
            mcp,
            node,
        } = self;
        match talker.and(chatter).and(coder) {
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
    chat_certificate: chat::ClientCertificateFingerprint,
    status: SetupStatus,
    security: Option<manifest::SecurityPolicy>,
}

impl Setup {
    /// Returns the provisioned member identity.
    #[must_use]
    pub const fn did(&self) -> Did {
        self.did
    }

    /// Returns the persistent IRC client certificate fingerprint.
    #[must_use]
    pub const fn chat_certificate(&self) -> chat::ClientCertificateFingerprint {
        self.chat_certificate
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
        writeln!(
            formatter,
            "{} IRC certificate: {}",
            self.member, self.chat_certificate
        )?;
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
                "add this binding to radicle-team.toml:\n\n[members.{}]\nsigning_did = \
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
    /// The repository has no `radicle-team.toml` manifest.
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
    /// The member directory does not belong to the member being run.
    #[error("'{path}' is not this member's directory: {reason}")]
    MemberDirectory {
        /// Member directory path.
        path: Box<Path>,
        /// Why it was refused.
        reason: String,
    },
    /// The conversation dispatcher stopped.
    #[error("conversation dispatcher error: {0}")]
    Conversation(String),
    /// Preparing the member's working copies failed.
    #[error("workspace error: {0}")]
    Workspace(String),
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
    #[test]
    fn refuses_a_directory_belonging_to_another_member_or_repository() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository =
            RepoId::from_urn("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("repository should parse");
        let other =
            RepoId::from_urn("rad:z42hL2jL4Tn8K5qgKbQ3xYrxLX9rU").expect("repository should parse");
        let home = crate::home::MemberHome::at(root.path().join("nico"));
        home.create().expect("member directory should be created");
        home.record(crate::member::Member::Nico, repository)
            .expect("marker should be recorded");

        super::Application::new(crate::member::Member::Nico, repository)
            .validate_member_home(&home)
            .expect("its own directory must be accepted");

        for wrong in [
            super::Application::new(crate::member::Member::Maya, repository),
            super::Application::new(crate::member::Member::Nico, other),
        ] {
            assert!(
                matches!(
                    wrong.validate_member_home(&home),
                    Err(super::Error::MemberDirectory { .. })
                ),
                "running in another member's directory would publish under the wrong identity"
            );
        }

        let empty = crate::home::MemberHome::at(root.path().join("empty"));
        empty.create().expect("directory should be created");
        assert!(
            matches!(
                super::Application::new(crate::member::Member::Nico, repository)
                    .validate_member_home(&empty),
                Err(super::Error::Home(_))
            ),
            "a directory with no marker is not a member directory"
        );
    }

    // The marker is written before the identity is created, so a directory holding one without
    // the other is a member's whose marker was lost. Claiming it writes this member's name over
    // it, and the identity loaded a moment later is then somebody else's key under this member's
    // name.
    #[cfg(not(miri))]
    #[test]
    fn refuses_to_claim_a_directory_which_already_holds_an_identity() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository =
            RepoId::from_urn("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("repository should parse");
        let home = crate::home::MemberHome::at(root.path().join("unmarked"));
        home.create().expect("member directory should be created");
        fs::create_dir_all(home.identity()).expect("identity directory should be created");

        let claimed =
            super::Application::new(crate::member::Member::Nico, repository).claim_home(&home);

        assert!(
            matches!(claimed, Err(super::Error::MemberDirectory { .. })),
            "an identity with no marker must be refused rather than adopted: {claimed:?}"
        );
        assert!(
            !home.path().join(crate::home::MARKER).is_file(),
            "and nothing may be written into it"
        );
    }
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
            chat_certificate: crate::chat::ClientCertificateFingerprint::from_digest([1; 32]),
            status: SetupStatus::ManifestMissing { generation: 1 },
            security: None,
        };
        let output = setup.to_string();

        assert!(output.contains("nico identity: did:key:"));
        assert!(output.contains("nico IRC certificate: cert-sha256:"));
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
        let home = MemberHome::at(&root);
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
