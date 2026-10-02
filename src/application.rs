//! Application orchestration for one Radicle team member.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};

use radicle::{
    identity::{Did, RepoId},
    node::{Node, NodeId, policy::Scope, seed::Store as _},
    profile::Profile as RadicleProfile,
    storage::{ReadRepository as _, ReadStorage as _},
};
use tokio::sync::watch;

use crate::{
    agent::{self, Chatter, Coder, Talker, adapter, documents},
    builder, chat, conversation,
    home::{self, MemberHome},
    identity::{self, GenerationStore, Manager, MemberLock, Provider},
    manifest::{self, Manifest},
    mcp,
    member::Member,
    node::{self, NodeManager},
    provider::{self, credentials},
    queue::WorkQueue,
    sandbox::AgentSandbox,
    shutdown::Shutdown,
    watcher,
};

const SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const SYNC_ATTEMPTS: usize = 3;
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(1);
const ACTIVATION_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A single Radicle team member application.
#[derive(Debug, Clone)]
pub struct Application {
    member: Member,
    repository: RepoId,
    home: Option<PathBuf>,
    identity_provider: Provider,
    manifest_file: Option<PathBuf>,
    provider: Option<String>,
    seeds: Vec<NodeId>,
    shutdown: Shutdown,
    setup_progress: Option<watch::Sender<String>>,
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
            provider: None,
            seeds: Vec::new(),
            shutdown: Shutdown::default(),
            setup_progress: None,
        }
    }

    pub(crate) fn with_shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    pub(crate) fn with_setup_progress(mut self, progress: watch::Sender<String>) -> Self {
        self.setup_progress = Some(progress);
        self
    }

    fn report_setup(&self, message: impl Into<String>) {
        let message = message.into();
        if let Some(progress) = &self.setup_progress {
            tracing::debug!(%message, "setup progress");
            progress.send_replace(message);
        } else {
            tracing::info!(member = %self.member, "{message}");
        }
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

    /// Runs on `provider` instead of the one the manifest names.
    ///
    /// The operator running a member pays for it and holds its credentials, so their choice wins
    /// over the team's recorded intent.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Returns the model provider this run was told to use, if any.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Returns the provider this member runs on, preferring the command line over the manifest.
    ///
    /// Neither is required. Without one the agent application uses whatever it is configured with,
    /// which is what a first member set up before any provider was chosen needs.
    #[must_use]
    pub fn resolved_provider(&self, binding: &manifest::ValidatedBinding) -> Option<String> {
        self.provider
            .clone()
            .or_else(|| binding.provider().map(str::to_owned))
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

    /// Creates or loads the member identity without starting a node or fetching a repository.
    ///
    /// The saved identity is reused by subsequent setup, so its DID can be granted access to a
    /// private repository before synchronization is attempted.
    ///
    /// # Errors
    ///
    /// Returns an error when the member directory, process lock, or identity cannot be prepared.
    pub async fn setup_identity(&self) -> Result<Did, Error> {
        let home = self.member_home()?;
        Self::provision_identity(
            self.member,
            Some(self.repository),
            home,
            self.identity_provider.clone(),
        )
        .await
    }

    pub(crate) async fn provision_identity(
        member: Member,
        repository: Option<RepoId>,
        home: MemberHome,
        provider: Provider,
    ) -> Result<Did, Error> {
        tokio::task::spawn_blocking(move || {
            let _lock = MemberLock::acquire(&home)?;
            Self::claim_identity_home(&home, member, repository)?;
            let identity = Manager::default().load(member, &home, &provider)?;
            Ok(identity.did())
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
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
        self.report_setup("Stopping setup's Radicle node");
        let stopped = runtime.stop(&self.shutdown).await;

        Self::finish_after_stop(setup, stopped)
    }

    /// Takes the member directory the way `ownership` asks for, under the process lock.
    fn take_home(&self, home: &MemberHome, ownership: Ownership) -> Result<(), Error> {
        match ownership {
            Ownership::Claim => Self::claim_identity_home(home, self.member, Some(self.repository)),
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
    fn claim_identity_home(
        home: &MemberHome,
        member: Member,
        repository: Option<RepoId>,
    ) -> Result<(), Error> {
        if home.path().join(crate::home::MARKER).is_file() {
            Self::validate_home_owner(home, member, repository)?;
            if repository.is_none() {
                return Ok(());
            }
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

        match repository {
            Some(repository) => home.record(member, repository),
            None => home.record_identity(member),
        }
        .map_err(Error::from)
    }

    async fn prepare_setup(&self, runtime: &Runtime) -> Result<Setup, Error> {
        self.synchronize(&runtime.identity).await?;
        self.report_setup("Checking repository configuration");
        let (status, security) = self.setup_status(runtime).await?;
        let did = runtime.identity.did();
        self.report_setup("Preparing chat identity");
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
    /// Waits for the canonical manifest to authorize this identity before starting agents when the
    /// repository manifest or member binding is still missing.
    ///
    /// Each of the member's three roles is started as its own agent on the provider this member
    /// runs on: the runtime holds each one's process, so it can stop an agent which has gone silent
    /// or refused to stop.
    ///
    /// The three share one identity, one manifest binding, and one node, but not one train of
    /// thought. A half-written patch has no safe point to pause at, and an agent turn cannot be
    /// interrupted, so neither a queued job nor a review may hold up somebody waiting in a channel.
    ///
    /// Enabling chat therefore costs a third concurrent agent for this member whenever a message
    /// arrives, on top of the talking and building roles.
    ///
    /// Linux executables must call
    /// [`sandbox::dispatch_linux_helper`](crate::sandbox::dispatch_linux_helper) as the first
    /// statement in `main`.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider cannot be reached, or if preparation, watching, building,
    /// or agent dispatch fails.
    pub async fn run(&self) -> Result<(), Error> {
        let (runtime, binding) = self.start_run().await?;
        let mut roles = match self.roles(&runtime, &binding).await {
            Ok(roles) => roles,
            Err(error) => return self.abandon_run(runtime, error).await,
        };

        Box::pin(self.run_roles(
            runtime,
            &binding,
            &mut *roles.talker,
            &mut *roles.chatter,
            &mut *roles.coder,
        ))
        .await
    }

    /// Builds one agent per role for the provider this member was told to run on.
    ///
    /// A provider is a file: its adapter, its models, and its settings are all strings an operator
    /// may edit. Everything which can be checked before an agent starts is checked here, so a
    /// mistake is reported at startup rather than on the first delivery. Which agent application
    /// ends up behind the roles is [`crate::agent::adapter::select`]'s business.
    async fn roles(
        &self,
        runtime: &Runtime,
        binding: &manifest::ValidatedBinding,
    ) -> Result<adapter::Roles, Error> {
        let (configuration, name, selected) = self.selected(binding)?;
        let held = self
            .authorization(&configuration, runtime, &selected, &name)
            .await?;
        let roles = adapter::select(selected.adapter(), &selected, held.as_ref())?;
        tracing::info!(
            provider = name,
            adapter = selected.adapter(),
            "running on provider"
        );

        Ok(roles)
    }

    /// Returns the provider this member runs on, with the directory it was read from.
    fn selected(
        &self,
        binding: &manifest::ValidatedBinding,
    ) -> Result<(PathBuf, String, provider::Provider), Error> {
        let configuration = provider::configuration()?;
        let name = self
            .resolved_provider(binding)
            .ok_or(Error::NoModelProvider)?;
        let selected = provider::find(&provider::directory(&configuration), &name)?;

        Ok((configuration, name, selected))
    }

    /// Returns what this member reaches `name` with, refusing a provider nobody has authorized.
    ///
    /// The member's own store wins over the machine's, which is how one project is billed to an
    /// account of its own.
    async fn authorization(
        &self,
        configuration: &Path,
        runtime: &Runtime,
        selected: &provider::Provider,
        name: &str,
    ) -> Result<Option<provider::Credentials>, Error> {
        let held = credentials::load(
            &credentials::directory(configuration),
            Some(&credentials::directory(runtime.home.path())),
            name,
        )?;
        let authorized =
            agent::authentication::Authentication::new(selected, self.member, &runtime.home)
                .authorized(held.as_ref())
                .await
                .map_err(|source| Error::ProviderAuthorization {
                    provider: name.into(),
                    home: runtime.home.path().into(),
                    source,
                })?;
        if !authorized {
            return Err(Error::UnauthorizedProvider {
                provider: name.into(),
                home: runtime.home.path().into(),
            });
        }

        Ok(held)
    }

    async fn run_roles(
        &self,
        runtime: Runtime,
        binding: &manifest::ValidatedBinding,
        talker: &mut (impl Talker + ?Sized),
        chatter: &mut (impl Chatter + ?Sized),
        coder: &mut (impl Coder + ?Sized),
    ) -> Result<(), Error> {
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            title = profile.title,
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
            .wait_for_binding(&runtime.home, &runtime.identity)
            .await?;
        self.prepare_security_repository(runtime, binding.security())
            .await?;

        Ok(binding)
    }

    // The proposed manifest is local setup state. Only canonical (or explicitly selected
    // administrator-owned) policy may authorize agents to start.
    async fn wait_for_binding(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<manifest::ValidatedBinding, Error> {
        if self.manifest_file.is_some() {
            return self
                .validate_profile_binding(home, identity.profile(), identity.did())
                .await;
        }
        let mut waiting = false;
        loop {
            self.ensure_running()?;
            if let SetupValidation::Ready(binding) = self.load_setup_validation(identity).await? {
                self.record_binding(home, (*binding).clone()).await?;
                return Ok(*binding);
            }
            if !waiting {
                tracing::warn!(member = %self.member, repository = %self.repository,
                    "awaiting manifest approval on the canonical branch; agents have not started");
                waiting = true;
            }
            tokio::time::sleep(ACTIVATION_POLL_INTERVAL).await;
            self.ensure_running()?;
            match self.synchronize(identity).await {
                Ok(()) => {}
                Err(error @ (Error::Sync { .. } | Error::SyncTimeout { .. })) => {
                    tracing::warn!(%error, "synchronization failed while waiting for manifest approval");
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn announce_manifest_patch(&self) -> Result<(), Error> {
        let runtime = Runtime::start(self, Ownership::Verify).await?;
        self.report_setup("Replicating manifest patch to a repository seed");
        // `announce_refs_for` only queues an announcement. Keep the node alive while the CLI
        // waits, then verify its acknowledgement: `rad sync` can succeed with no seeds.
        let result = async {
            let output = Self::sync_output(
                || Ok(self.announce_command(&runtime.identity)),
                self.repository,
                runtime.identity.did(),
                SYNC_TIMEOUT,
            )
            .await?;
            self.ensure_running()?;
            self.validate_sync_output(&runtime.identity, self.repository, output)?;
            let profile = runtime.identity.profile().clone();
            let repository = self.repository;
            tokio::task::spawn_blocking(move || Self::confirm_replication(&profile, repository))
                .await
                .map_err(|error| Error::Task(error.to_string()))
                .flatten()
        }
        .await;
        Self::finish_after_stop(result, runtime.stop(&self.shutdown).await)
    }

    fn announce_command(&self, identity: &identity::Identity) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("rad");
        command.args(["sync", "--announce", "--replicas", "1", "--timeout", "30s"]);
        for seed in &self.seeds {
            command.arg("--seed").arg(seed.to_string());
        }
        command.arg(self.repository.to_string());
        identity.configure_radicle_command(command.as_std_mut());
        command
    }

    fn confirm_replication(profile: &RadicleProfile, repository: RepoId) -> Result<(), Error> {
        let stored = profile.storage.repository(repository)?;
        let refs = radicle::storage::refs::RefsAt::new(&stored, *profile.id())?;
        let database = profile.database()?;
        for seed in database.seeds_for(&repository)? {
            let seed = seed?;
            if seed.nid != *profile.id() && seed.synced_at.oid == refs.at {
                return Ok(());
            }
        }
        Err(Error::ReplicationUnconfirmed {
            repository,
            did: profile.did(),
        })
    }

    // Resolves this member's directory.
    pub(crate) fn member_home(&self) -> Result<MemberHome, home::Error> {
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
    pub(crate) fn validate_member_home(&self, home: &MemberHome) -> Result<(), Error> {
        Self::validate_home_owner(home, self.member, Some(self.repository))?.ok_or_else(|| {
            Error::MemberDirectory {
                path: home.path().into(),
                reason: "its identity is not yet bound to a repository; complete setup with --repo"
                    .to_owned(),
            }
        })?;
        Ok(())
    }

    fn validate_home_owner(
        home: &MemberHome,
        expected_member: Member,
        expected_repository: Option<RepoId>,
    ) -> Result<Option<RepoId>, Error> {
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
        if member != expected_member
            || repository
                .zip(expected_repository)
                .is_some_and(|(actual, expected)| actual != expected)
        {
            return Err(Error::MemberDirectory {
                path: home.path().into(),
                reason: match repository {
                    Some(repository) => format!("it belongs to {member} in {repository}"),
                    None => format!("it belongs to {member}"),
                },
            });
        }

        Ok(repository)
    }

    /// Returns the directory `setup` creates when none was given: member and repository when
    /// selected, or just the member when preparing an identity without a repository.
    pub(crate) fn default_setup_home(
        member: Member,
        repository: Option<RepoId>,
    ) -> Result<MemberHome, home::Error> {
        let working = std::env::current_dir().map_err(home::Error::WorkingDirectory)?;
        let name = match repository {
            Some(repository) => {
                let urn = repository.urn();
                let short: String = urn
                    .strip_prefix("rad:")
                    .unwrap_or(&urn)
                    .chars()
                    .take(8)
                    .collect();
                format!("{}-{short}", member.id())
            }
            None => member.id().to_owned(),
        };

        Ok(MemberHome::at(working.join(name)))
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
        self.report_setup(format!("Synchronizing {repository}"));
        // `rad clone --bare` still creates a Git checkout. Keep that bootstrap checkout temporary;
        // the member's durable repository lives in its Radicle storage.
        let temporary = tempfile::tempdir().map_err(Error::SyncSpawn)?;
        let checkout = temporary.path().join("checkout.git");
        let output = Self::sync_output(
            || self.sync_command(identity, repository, &checkout),
            repository,
            identity.did(),
            SYNC_TIMEOUT,
        )
        .await?;
        self.ensure_running()?;
        self.validate_sync_output(identity, repository, output)
    }

    // Radicle's --timeout applies to individual fetches, not the whole subprocess. Keep the overall
    // wait bounded as well, and terminate the subprocess when that deadline expires.
    async fn sync_output(
        command: impl FnMut() -> Result<tokio::process::Command, Error>,
        repository: RepoId,
        did: Did,
        timeout: Duration,
    ) -> Result<std::process::Output, Error> {
        tokio::time::timeout(timeout, Self::sync_attempts(command))
            .await
            .map_err(|_elapsed| Error::SyncTimeout {
                repository,
                did,
                timeout,
            })?
    }

    async fn sync_attempts(
        mut command: impl FnMut() -> Result<tokio::process::Command, Error>,
    ) -> Result<std::process::Output, Error> {
        let mut attempts = 0;
        loop {
            let output = command()?
                .kill_on_drop(true)
                .output()
                .await
                .map_err(Error::SyncSpawn)?;
            attempts += 1;
            if attempts == SYNC_ATTEMPTS || !Self::sync_node_busy(&output) {
                return Ok(output);
            }
            // A fresh node can spend longer than the control timeout validating the initial network
            // announcements. Keep it running so the next fetch can use that work.
            tracing::debug!(
                attempts,
                "Radicle control request timed out; retrying synchronization"
            );
            tokio::time::sleep(SYNC_RETRY_DELAY).await;
        }
    }

    fn sync_node_busy(output: &std::process::Output) -> bool {
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr)
                .contains("timed out reading from control socket")
    }

    fn sync_command(
        &self,
        identity: &identity::Identity,
        repository: RepoId,
        checkout: &Path,
    ) -> Result<tokio::process::Command, Error> {
        let mut command = tokio::process::Command::new("rad");
        let stored = identity.profile().storage.contains(&repository)?;
        if stored {
            command.args(["sync", "--fetch", "--replicas", "1"]);
        } else {
            command.args(["clone", "--bare"]);
        }
        command.args(["--timeout", "30s"]);
        for seed in &self.seeds {
            command.arg("--seed").arg(seed.to_string());
        }
        command.arg(repository.to_string());
        if !stored {
            command.arg(checkout);
        }
        identity.configure_radicle_command(command.as_std_mut());

        Ok(command)
    }

    fn validate_sync_output(
        &self,
        identity: &identity::Identity,
        repository: RepoId,
        output: std::process::Output,
    ) -> Result<(), Error> {
        if output.status.success() && identity.profile().storage.contains(&repository)? {
            Ok(())
        } else {
            let mut details = if output.stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            };
            if output.status.success() {
                details =
                    format!("repository was not fetched into the member's storage; {details}");
            }
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
        let (lock, identity) = Self::prepare_identity(application, home, ownership).await?;
        let node = Self::start_node(application, home, &identity).await?;

        Ok((lock, identity, node))
    }

    async fn prepare_identity(
        application: &Application,
        home: &MemberHome,
        ownership: Ownership,
    ) -> Result<(MemberLock, identity::Identity), Error> {
        let worker_home = home.clone();
        let worker_application = application.clone();
        let (lock, identity) = tokio::task::spawn_blocking(move || {
            let lock = MemberLock::acquire(&worker_home)?;
            worker_application.take_home(&worker_home, ownership)?;
            worker_application
                .report_setup("Unlocking identity — approve keychain access if prompted");
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
        Ok((lock, identity))
    }

    async fn start_node(
        application: &Application,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<NodeManager, Error> {
        application.report_setup("Starting member's Radicle node");
        let node = NodeManager::start_supervised(identity, home, &application.shutdown).await?;
        match Self::prepare_node(application, identity, &node).await {
            Ok(()) => Ok(node),
            Err(error) => {
                application.report_setup("Stopping member's Radicle node");
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
        application.seed_repository(identity, node.node()).await?;
        application.report_setup("Connecting to repository seeds");
        node.connect_seeds(identity, application.repository, &application.shutdown)
            .await?;
        application.ensure_running()
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
    // Absent when the manifest has no chat section, so a repository without chat runs two roles
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

    // Creates the shared object store and every role's working copy.
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

    /// Starts one MCP server for one role.
    async fn start_server(
        storage: radicle::storage::git::Storage,
        repository: RepoId,
        publisher: agent::Publisher,
        allowlist: manifest::McpAllowlist,
        role: agent::Role,
        state: mcp::ToolState,
    ) -> Result<mcp::Server, Error> {
        mcp::Server::start(storage, repository, publisher, allowlist, role, state)
            .await
            .map_err(|error| Error::Mcp(error.to_string()))
    }

    // Lists the servers to start.
    fn wanted_servers(chat: bool) -> Vec<agent::Role> {
        match chat {
            true => vec![
                agent::Role::Talker,
                agent::Role::Chatter,
                agent::Role::Coder,
            ],
            false => vec![agent::Role::Talker, agent::Role::Coder],
        }
    }

    // Starts every wanted server, stopping the ones which did start if a later one fails.
    async fn opened<F>(
        wanted: Vec<agent::Role>,
        start: impl Fn(agent::Role) -> F,
    ) -> Result<Vec<(agent::Role, mcp::Server)>, Error>
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

    // Sorts the started servers back into the roles they belong to.
    fn assembled(mut started: Vec<(agent::Role, mcp::Server)>) -> Result<Servers, Error> {
        let mut take = |wanted| {
            started
                .iter()
                .position(|(role, _server)| *role == wanted)
                .map(|found| started.remove(found).1)
        };
        let missing = || Error::Mcp("an MCP server did not start".to_owned());

        Ok(Servers {
            talker: take(agent::Role::Talker).ok_or_else(missing)?,
            chatter: take(agent::Role::Chatter),
            coder: take(agent::Role::Coder).ok_or_else(missing)?,
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
        let (sandbox, layout) =
            Self::prepared(application, runtime, agent::Role::Talker, &servers.talker)?;
        talker
            .start(profile, servers.talker.endpoint(), sandbox, layout)
            .await?;
        Self::start_chatter(application, runtime, servers.chatter.as_ref(), chatter).await?;
        let (sandbox, layout) =
            Self::prepared(application, runtime, agent::Role::Coder, &servers.coder)?;
        coder
            .start(profile, servers.coder.endpoint(), sandbox, layout)
            .await
            .map_err(Error::from)
    }

    // Starts the chatting role, if the manifest asked for chat at all.
    async fn start_chatter(
        application: &Application,
        runtime: &Runtime,
        server: Option<&mcp::Server>,
        chatter: &mut (impl Chatter + ?Sized),
    ) -> Result<(), Error> {
        let Some(server) = server else {
            return Ok(());
        };
        let (sandbox, layout) = Self::prepared(application, runtime, agent::Role::Chatter, server)?;

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

    // Prepares one role's sandbox and its fixed directories.
    fn prepared(
        application: &Application,
        runtime: &Runtime,
        role: agent::Role,
        server: &mcp::Server,
    ) -> Result<(AgentSandbox, agent::Layout), Error> {
        let sandbox =
            AgentSandbox::load(&runtime.home, application.member, role, server.endpoint())?;
        // The documents are compiled in, so a role is instructed by this build rather than by
        // whatever the last one left behind. Written here rather than by the adapter, because
        // installing empties the directory first and only the member's home may say which
        // directory that is.
        let documents = documents::install(&runtime.home, application.member, role)
            .map_err(Error::Documents)?;
        tracing::debug!(
            %role,
            documents = documents.len(),
            "installed the member's standing instructions"
        );

        Ok((
            sandbox,
            agent::Layout::new(
                role,
                runtime.home.cache(),
                runtime.home.harness(role),
                documents,
            ),
        ))
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
    /// The Radicle clone or sync subprocess could not be started.
    #[error("failed to start Radicle repository synchronization: {0}")]
    SyncSpawn(io::Error),
    /// The complete Radicle clone or sync subprocess exceeded its deadline.
    #[error("synchronizing {repository} as {did} timed out after {timeout:?}; retry setup")]
    SyncTimeout {
        /// Repository being synchronized.
        repository: RepoId,
        /// Member identity used for synchronization.
        did: Did,
        /// Maximum duration of the complete subprocess.
        timeout: Duration,
    },
    /// Repository synchronization failed.
    #[error("failed to synchronize {repository} as {did} ({status}): {details}")]
    Sync {
        /// Repository being synchronized.
        repository: RepoId,
        /// Member identity used for synchronization.
        did: Did,
        /// Subprocess exit status.
        status: ExitStatus,
        /// Diagnostic output from Radicle.
        details: String,
    },
    /// No remote seed acknowledged the member's current signed references.
    #[error("no seed confirmed replication of {repository} as {did}; retry setup")]
    ReplicationUnconfirmed {
        /// Repository whose patch must be replicated.
        repository: RepoId,
        /// Member whose signed references must be fetched.
        did: Did,
    },
    /// Reading the member's current signed references failed.
    #[error("failed to read signed references: {0}")]
    SignedReferences(#[from] radicle::storage::refs::sigrefs::read::error::Read),
    /// Opening the node's replication database failed.
    #[error("failed to open node replication database: {0}")]
    NodeDatabase(#[from] radicle::node::db::Error),
    /// Reading seed replication acknowledgements failed.
    #[error("failed to read seed replication acknowledgements: {0}")]
    SeedReplication(#[from] radicle::node::seed::Error),
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
    /// Reading the model provider failed.
    #[error(transparent)]
    Provider(#[from] crate::provider::Error),
    /// Reading what the model provider is reached with failed.
    #[error(transparent)]
    Credentials(#[from] crate::provider::CredentialsError),
    /// Nothing says which models this member runs on.
    #[error(
        "no model provider chosen for this member; run `rad-team provider select` from the member \
         directory and merge its manifest patch"
    )]
    NoModelProvider,
    /// The provider names an agent application this build has no adapter for.
    #[error(transparent)]
    UnknownAdapter(#[from] agent::adapter::Unknown),
    /// The provider needs authorizing and nothing on this machine has authorized it.
    #[error(
        "provider '{provider}' is not authorized for this member; run `rad-team provider login \
         --home '{home}'`"
    )]
    UnauthorizedProvider {
        /// Selected provider.
        provider: String,
        /// Member directory to authorize.
        home: PathBuf,
    },
    /// Native provider authorization could not be checked.
    #[error(
        "could not check provider '{provider}': {source}; run `rad-team provider login --home \
         '{home}'`"
    )]
    ProviderAuthorization {
        /// Selected provider.
        provider: String,
        /// Member directory to authorize.
        home: PathBuf,
        /// Native adapter failure.
        source: agent::authentication::Error,
    },
    /// Writing a role's standing instructions failed.
    #[error("could not install the member's instructions: {0}")]
    Documents(#[source] io::Error),
    /// Starting or stopping the agent role failed.
    #[error(transparent)]
    Talker(#[from] agent::Error),
    /// A blocking filesystem or Radicle task failed.
    #[error("application task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {

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

    #[tokio::test]
    async fn full_setup_binds_a_prepared_identity_without_replacing_it() {
        use super::{Ownership, Runtime};
        use crate::identity::{KeyProtection, Provider};

        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(root.path().join("maya"));
        let provider = Provider::Managed {
            protection: KeyProtection::Unencrypted,
        };
        let did =
            Application::provision_identity(Member::Maya, None, home.clone(), provider.clone())
                .await
                .expect("identity should be provisioned without a repository");
        let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
            .parse()
            .expect("RID should parse");
        let application = Application::new(Member::Maya, repository)
            .with_home(home.path())
            .with_identity_provider(provider);
        assert!(
            matches!(
                application.validate_member_home(&home),
                Err(super::Error::MemberDirectory { .. })
            ),
            "run must refuse an identity without a repository binding"
        );

        let (lock, identity) = Runtime::prepare_identity(&application, &home, Ownership::Claim)
            .await
            .expect("full setup's first stage should bind the prepared identity");
        assert_eq!(identity.did(), did);
        assert_eq!(
            home.marker()
                .expect("marker should exist")
                .repository()
                .expect("repository should parse"),
            Some(repository)
        );
        application
            .validate_member_home(&home)
            .expect("the bound identity should be usable by run");
        assert!(
            !home.node_log().exists(),
            "binding should precede node startup"
        );
        drop(lock);

        let other = "rad:z42hL2jL4Tn8K5qgKbQ3xYrxLX9rU"
            .parse()
            .expect("other RID should parse");
        assert!(
            matches!(
                Application::new(Member::Maya, other).take_home(&home, Ownership::Claim),
                Err(super::Error::MemberDirectory { .. })
            ),
            "an existing binding must not be overwritten by another repository"
        );
    }

    // The manifest records what the team agreed; the operator running the member pays for it and
    // holds the credentials. So a command-line choice wins, and neither being set is normal.
    #[test]
    fn unauthorized_provider_reports_the_member_local_login_command() {
        let error = crate::application::Error::UnauthorizedProvider {
            provider: "gpt".into(),
            home: "/team/maya".into(),
        };
        assert!(
            error
                .to_string()
                .contains("rad-team provider login --home '/team/maya'")
        );
    }

    #[test]
    fn prefers_the_command_lines_provider_over_the_manifests() {
        let repository =
            RepoId::from_urn("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("repository should parse");
        let application = super::Application::new(crate::member::Member::Nico, repository);
        let named = |provider: Option<&str>| {
            crate::manifest::ValidatedBinding::new(
                crate::member::Member::Nico,
                &crate::manifest::Binding::test(
                    "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
                        .parse()
                        .expect("DID should parse"),
                    1,
                )
                .with_provider(provider),
                crate::manifest::Revision::External,
                crate::manifest::SecurityPolicy::test(repository),
                crate::manifest::RepositoryPolicy::new(
                    crate::manifest::TriggerAccess::Delegates,
                    Vec::new(),
                    crate::manifest::PriorityLabels::default(),
                    crate::manifest::Limits::default(),
                ),
            )
        };

        assert_eq!(application.resolved_provider(&named(None)), None);
        assert_eq!(
            application.resolved_provider(&named(Some("anthropic"))),
            Some("anthropic".to_owned()),
            "the manifest decides when the operator did not"
        );
        assert_eq!(
            application
                .clone()
                .with_provider("ollama")
                .resolved_provider(&named(Some("anthropic"))),
            Some("ollama".to_owned()),
            "and the operator overrides it, because they hold the credentials"
        );
    }

    // The marker is written before the identity is created, so a directory holding one without the
    // other is a member's whose marker was lost. Claiming it writes this member's name over it, and
    // the identity loaded a moment later is then somebody else's key under this member's name.
    #[test]
    fn refuses_to_claim_a_directory_which_already_holds_an_identity() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository =
            RepoId::from_urn("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("repository should parse");
        let home = crate::home::MemberHome::at(root.path().join("unmarked"));
        home.create().expect("member directory should be created");
        fs::create_dir_all(home.identity()).expect("identity directory should be created");

        let claimed = super::Application::new(crate::member::Member::Nico, repository)
            .take_home(&home, super::Ownership::Claim);

        assert!(
            matches!(claimed, Err(super::Error::MemberDirectory { .. })),
            "an identity with no marker must be refused rather than adopted: {claimed:?}"
        );
        assert!(
            !home.path().join(crate::home::MARKER).is_file(),
            "and nothing may be written into it"
        );
    }
    use std::{fs, path::PathBuf, str::FromStr as _};

    use radicle::{
        crypto::Seed,
        git::Oid,
        identity::{RepoId, doc::Visibility},
        node::{Alias, Timestamp, seed::Store as _},
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };

    use crate::{
        Application, Setup, SetupStatus,
        application::SetupValidation,
        home::MemberHome,
        manifest::{Revision, ValidatedBinding},
        member::Member,
    };

    #[test]
    fn requires_a_remote_acknowledgement_of_the_current_signed_references() {
        let root = tempfile::tempdir().expect("fixture directory should exist");
        let profile = Profile::init(
            Home::new(root.path().join("identity")).expect("identity home should create"),
            Alias::new("replication-test"),
            None,
            Seed::generate(),
        )
        .expect("identity should create");
        let checkout = git2::Repository::init(root.path().join("checkout"))
            .expect("checkout should initialize");
        checkout
            .set_head("refs/heads/main")
            .expect("main should select");
        let tree = checkout
            .treebuilder(None)
            .expect("tree builder should create")
            .write()
            .expect("tree should write");
        let tree = checkout.find_tree(tree).expect("tree should read");
        let signature =
            git2::Signature::now("Test", "test@example.com").expect("signature should create");
        checkout
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .expect("commit should create");
        let (repository, _, _) = radicle::rad::init(
            &checkout,
            "replication-test".try_into().expect("name should parse"),
            "Replication test",
            "main".try_into().expect("branch should parse"),
            Visibility::Public,
            &profile.signer().expect("signer should load"),
            &profile.storage,
        )
        .expect("repository should initialize");
        let stored = profile
            .storage
            .repository(repository)
            .expect("storage should open");
        let refs = radicle::storage::refs::RefsAt::new(&stored, *profile.id())
            .expect("signed references should read");
        let unconfirmed = || {
            assert!(matches!(
                Application::confirm_replication(&profile, repository),
                Err(super::Error::ReplicationUnconfirmed { repository: rid, did })
                    if rid == repository && did == profile.did()
            ));
        };
        unconfirmed();
        let mut database = profile.database_mut().expect("database should open");
        database
            .synced(&repository, profile.id(), refs.at, Timestamp::EPOCH + 1)
            .expect("local record should write");
        unconfirmed();
        let remote = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            .parse()
            .expect("remote node should parse");
        database = database
            .init(
                &remote,
                radicle::node::Features::SEED,
                &Alias::new("seed"),
                &radicle::node::UserAgent::default(),
                Timestamp::EPOCH,
                [],
            )
            .expect("remote node should register");
        database
            .synced(
                &repository,
                &remote,
                Oid::from_sha1([0; 20]),
                Timestamp::EPOCH + 1,
            )
            .expect("stale acknowledgement should write");
        unconfirmed();
        database
            .synced(&repository, &remote, refs.at, Timestamp::EPOCH + 2)
            .expect("current acknowledgement should write");
        Application::confirm_replication(&profile, repository)
            .expect("current remote acknowledgement should satisfy replication");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminates_a_sync_subprocess_that_exceeds_its_deadline() {
        use std::time::Duration;

        let root = tempfile::tempdir().expect("temporary directory should be created");
        let pid_file = root.path().join("sync.pid");
        let command = || {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args(["-c", "echo $$ > \"$SYNC_TEST_PID\"; exec sleep 60"])
                .env("SYNC_TEST_PID", &pid_file);
            Ok(command)
        };
        let (repository, did) = sync_target();
        assert!(matches!(
            Application::sync_output(command, repository, did, Duration::from_secs(1)).await,
            Err(super::Error::SyncTimeout { repository: rid, did: signer, .. })
                if rid == repository && signer == did
        ));
        let pid = fs::read_to_string(&pid_file)
            .expect("subprocess should have started")
            .trim()
            .parse()
            .expect("PID should parse");
        let pid = rustix::process::Pid::from_raw(pid).expect("PID should be positive");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rustix::process::test_kill_process(pid) {
                    Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => {
                        assert_eq!(error, rustix::io::Errno::SRCH);
                        break;
                    }
                }
            }
        })
        .await
        .expect("timed-out sync must be killed and reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_sync_errors_and_output_within_the_deadline() {
        use std::time::Duration;

        let mut attempts = 0;
        let command = || {
            attempts += 1;
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", "echo unavailable >&2; exit 7"]);
            Ok(command)
        };
        let (repository, did) = sync_target();
        let output = Application::sync_output(command, repository, did, Duration::from_secs(1))
            .await
            .expect("command should start");
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stderr, b"unavailable\n");
        assert_eq!(
            attempts, 1,
            "only control-socket timeouts should be retried"
        );

        let missing = || {
            Ok(tokio::process::Command::new(
                "/nonexistent/radicle-team-sync-test",
            ))
        };
        assert!(matches!(
            Application::sync_output(missing, repository, did, Duration::from_secs(1)).await,
            Err(super::Error::SyncSpawn(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retries_sync_while_a_new_node_processes_network_announcements() {
        let (repository, did) = sync_target();
        let mut attempts = 0;
        let command = || {
            attempts += 1;
            let mut command = tokio::process::Command::new("/bin/sh");
            let script = if attempts == 1 {
                "echo 'node: timed out reading from control socket' >&2; exit 1"
            } else {
                "echo synchronized"
            };
            command.args(["-c", script]);
            Ok(command)
        };

        let output =
            Application::sync_output(command, repository, did, std::time::Duration::from_secs(5))
                .await
                .expect("sync should resume when the same node becomes responsive");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"synchronized\n");
        assert_eq!(attempts, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sync_retries_share_one_deadline() {
        let (repository, did) = sync_target();
        let mut attempts = 0;
        let command = || {
            attempts += 1;
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args([
                "-c",
                "echo 'node: timed out reading from control socket' >&2; exit 1",
            ]);
            Ok(command)
        };

        assert!(matches!(
            Application::sync_output(
                command,
                repository,
                did,
                std::time::Duration::from_millis(250)
            )
            .await,
            Err(super::Error::SyncTimeout { .. })
        ));
        assert_eq!(
            attempts, 1,
            "the total deadline must also cover the retry delay"
        );
    }

    #[cfg(unix)]
    fn sync_target() -> (RepoId, radicle::identity::Did) {
        (
            "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
                .parse()
                .expect("RID should parse"),
            "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
                .parse()
                .expect("DID should parse"),
        )
    }

    #[test]
    #[cfg(unix)]
    fn synchronization_requires_the_repository_to_be_fetched() {
        use std::{os::unix::process::ExitStatusExt as _, process::Output};

        use crate::identity::{KeyProtection, Manager, Provider};

        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let home = MemberHome::at(root.path());
        home.record(Member::Nico, repository)
            .expect("member should be recorded");
        let identity = Manager::default()
            .load(
                Member::Nico,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::Unencrypted,
                },
            )
            .expect("fresh member identity should be created");
        let application = Application::new(Member::Nico, repository);
        let checkout = root.path().join("bootstrap.git");
        let command = application
            .sync_command(&identity, repository, &checkout)
            .expect("first synchronization should be configured");
        assert_eq!(
            command.as_std().get_args().next(),
            Some(std::ffi::OsStr::new("clone")),
            "a fresh identity must clone before synchronizing an existing repository"
        );
        assert_eq!(
            command.as_std().get_args().last(),
            Some(checkout.as_os_str()),
            "even --bare creates a checkout, so cloning must not use the caller's directory"
        );

        // `rad sync` can exit successfully without fetching anything on a fresh node.
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"No seeds found".to_vec(),
            stderr: Vec::new(),
        };
        assert!(matches!(
            application.validate_sync_output(&identity, repository, output),
            Err(super::Error::Sync { repository: actual, did, details, .. })
                if actual == repository
                    && did == identity.did()
                    && details.contains("repository was not fetched")
                    && details.contains("No seeds found")
        ));
    }

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
    async fn activation_waits_for_canonical_approval_and_rejects_an_invalid_binding() {
        use radicle::{
            node::device::Device,
            storage::{ReadRepository as _, SignRepository as _, WriteRepository as _},
        };

        use crate::identity::{KeyProtection, Manager, Provider};

        let root = tempfile::tempdir().expect("fixture directory should exist");
        let home = MemberHome::at(root.path().join("maya"));
        home.create().expect("member home should create");
        home.record_identity(Member::Maya)
            .expect("identity owner should be recorded");
        let identity = Manager::default()
            .load(
                Member::Maya,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::Unencrypted,
                },
            )
            .expect("member identity should create");
        let signer = Device::from(identity.memory_signer().expect("signer should load")).boxed();
        let source =
            git2::Repository::init(root.path().join("source")).expect("source should initialize");
        source
            .set_head("refs/heads/main")
            .expect("main should be selected");
        let empty = source
            .treebuilder(None)
            .expect("tree builder should create")
            .write()
            .expect("tree should write");
        let tree = source.find_tree(empty).expect("tree should read");
        let signature =
            git2::Signature::now("Test", "test@example.com").expect("signature should create");
        source
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .expect("commit should create");
        let (rid, _, _) = radicle::rad::init(
            &source,
            "activation-test".try_into().expect("name should parse"),
            "Activation test",
            "main".try_into().expect("branch should parse"),
            Visibility::Public,
            &signer,
            &identity.profile().storage,
        )
        .expect("Radicle repository should create");
        let application = Application::new(Member::Maya, rid).with_home(home.path());
        let manifest = format!(
            r#"version = 1
[security]
repository = "{rid}"
recipients = ["age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa"]
[members.maya]
signing_did = "{}"
generation = 1
state = "active"
mcp_allow = ["read"]
"#,
            identity.did()
        );
        let coder = home.worktree(crate::agent::Role::Coder);
        fs::create_dir_all(&coder).expect("coder directory should create");
        fs::write(coder.join(crate::manifest::FILE_NAME), &manifest)
            .expect("proposed manifest should write");

        // The pending local proposal never authorizes the member. Waiting succeeds only once
        // the repository delegates' canonical branch contains a valid binding.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            application.wait_for_binding(&home, &identity),
        )
        .await
        .expect_err("an unapproved local proposal must keep waiting");
        let stored = identity
            .profile()
            .storage
            .repository(rid)
            .expect("storage should open");
        let base = stored
            .canonical_head()
            .expect("canonical head should read")
            .1;
        let parent = stored
            .raw()
            .find_commit(base.into())
            .expect("base commit should read");
        let mut builder = stored
            .raw()
            .treebuilder(Some(&parent.tree().expect("base tree should read")))
            .expect("tree builder should create");
        let blob = stored
            .raw()
            .blob(manifest.as_bytes())
            .expect("manifest blob should write");
        builder
            .insert(crate::manifest::FILE_NAME, blob, 0o100644)
            .expect("manifest should enter tree");
        let tree = stored
            .raw()
            .find_tree(builder.write().expect("tree should write"))
            .expect("tree should read");
        let reference = format!(
            "refs/namespaces/{}/refs/heads/main",
            identity.profile().id()
        );
        stored
            .raw()
            .commit(
                Some(&reference),
                &signature,
                &signature,
                "Approve member",
                &tree,
                &[&parent],
            )
            .expect("canonical merge should create");
        stored
            .sign_refs(&signer)
            .expect("delegate references should sign");
        let binding = application
            .wait_for_binding(&home, &identity)
            .await
            .expect("approved member should activate");
        assert_eq!(binding.generation(), 1);

        let invalid = manifest.replace("state = \"active\"", "state = \"inactive\"");
        let blob = stored
            .raw()
            .blob(invalid.as_bytes())
            .expect("revoked manifest should write");
        builder
            .insert(crate::manifest::FILE_NAME, blob, 0o100644)
            .expect("revocation should enter tree");
        let tree = stored
            .raw()
            .find_tree(builder.write().expect("tree should write"))
            .expect("tree should read");
        let current = stored
            .raw()
            .find_reference(&reference)
            .expect("canonical reference should read")
            .peel_to_commit()
            .expect("canonical commit should read");
        stored
            .raw()
            .commit(
                Some(&reference),
                &signature,
                &signature,
                "Revoke member",
                &tree,
                &[&current],
            )
            .expect("revocation should commit");
        stored.sign_refs(&signer).expect("references should sign");
        assert!(matches!(
            application.wait_for_binding(&home, &identity).await,
            Err(super::Error::Manifest(crate::manifest::Error::Inactive(
                Member::Maya
            )))
        ));
    }

    #[tokio::test]
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
            &crate::manifest::Binding::test(did, i64::MAX.unsigned_abs()),
            Revision::Repository(Oid::from_sha1([0; 20])),
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
