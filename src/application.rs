//! Application orchestration for one Radicle team member.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
};

use radicle::{
    identity::{Did, RepoId},
    node::{Node, NodeId, policy::Scope},
    storage::{ReadRepository as _, ReadStorage as _},
};

use crate::{
    home::{self, MemberHome},
    identity::{self, GenerationStore, Manager, MemberLock, Provider},
    manifest::{self, Manifest},
    member::Member,
    node::{self, NodeManager},
    shutdown::Shutdown,
    watcher,
};

/// A single Radicle team member application.
#[derive(Debug)]
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
    pub fn setup(&self) -> Result<Setup, Error> {
        let runtime = Runtime::start(self)?;
        let setup = self.prepare_setup(&runtime);
        let stopped = runtime.stop(&self.shutdown);

        Self::finish_after_stop(setup, stopped)
    }

    fn prepare_setup(&self, runtime: &Runtime) -> Result<Setup, Error> {
        self.synchronize(&runtime.identity)?;
        let status = self.setup_status(&runtime.home, &runtime.identity)?;
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
    pub fn run(&self) -> Result<(), Error> {
        let (runtime, binding) = self.start_run()?;
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            generation = binding.generation(),
            "team member initialized"
        );
        let result = watcher::Watcher::new(self.member, self.repository).run(
            runtime.identity.profile(),
            runtime.node.node(),
            &runtime.home.state_database(),
            |activity| {
                tracing::info!(?activity, "activity received");
            },
        );
        let interrupted = self.shutdown.is_requested();
        let stopped = runtime.stop(&self.shutdown);
        if interrupted {
            return stopped.map_err(Error::from);
        }
        result.map_err(Error::from)?;
        stopped.map_err(Error::from)
    }

    fn start_run(&self) -> Result<(Runtime, manifest::ValidatedBinding), Error> {
        let runtime = Runtime::start(self)?;
        let binding = self.prepare_run(&runtime);
        match binding {
            Ok(binding) => Ok((runtime, binding)),
            Err(error) => Self::finish_after_stop(Err(error), runtime.stop(&self.shutdown)),
        }
    }

    fn prepare_run(&self, runtime: &Runtime) -> Result<manifest::ValidatedBinding, Error> {
        self.synchronize(&runtime.identity)?;
        self.validate_binding(&runtime.home, &runtime.identity)
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

    fn synchronize(&self, identity: &identity::Identity) -> Result<(), Error> {
        self.ensure_running()?;
        let output = self
            .sync_command(identity)
            .output()
            .map_err(Error::SyncSpawn)?;
        self.ensure_running()?;
        self.validate_sync_output(identity, output)
    }

    fn sync_command(&self, identity: &identity::Identity) -> Command {
        let mut command = Command::new("rad");
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
        identity.configure_radicle_command(&mut command);

        command
    }

    fn validate_sync_output(
        &self,
        identity: &identity::Identity,
        output: Output,
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

    fn seed_repository(&self, identity: &identity::Identity, node: &Node) -> Result<(), Error> {
        let mut node = node.clone();
        identity
            .profile()
            .seed(self.repository, Scope::All, &mut node)?;

        Ok(())
    }

    fn ensure_running(&self) -> Result<(), Error> {
        if self.shutdown.is_requested() {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }

    fn setup_status(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<SetupStatus, Error> {
        let manifest = self.setup_manifest(identity)?;

        SetupValidation::new(manifest, self.member, identity)?.record(self, home)
    }

    fn setup_manifest(&self, identity: &identity::Identity) -> Result<Option<Manifest>, Error> {
        let repository = identity.profile().storage.repository(self.repository)?;
        self.validate_repository_access(&repository, identity.did())?;

        self.load_setup_manifest(&repository).map_err(Error::from)
    }

    fn next_binding_generation(&self, home: &MemberHome) -> Result<u64, Error> {
        GenerationStore::open(&home.state_database())?
            .next_generation(self.repository, self.member)
            .map_err(Error::from)
    }

    fn validate_binding(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let binding = self.load_validated_binding(identity)?;
        self.record_binding(home, binding)?;

        Ok(binding)
    }

    fn load_validated_binding(
        &self,
        identity: &identity::Identity,
    ) -> Result<manifest::ValidatedBinding, Error> {
        let repository = identity.profile().storage.repository(self.repository)?;
        self.validate_repository_access(&repository, identity.did())?;
        let manifest = self.load_manifest(&repository)?;

        manifest
            .validate(self.member, identity)
            .map_err(Error::from)
    }

    fn record_binding(
        &self,
        home: &MemberHome,
        binding: manifest::ValidatedBinding,
    ) -> Result<(), Error> {
        GenerationStore::open(&home.state_database())?
            .record(self.repository, binding)
            .map_err(Error::from)
    }

    fn validate_repository_access(
        &self,
        repository: &radicle::storage::git::Repository,
        did: Did,
    ) -> Result<(), Error> {
        if repository.identity_doc()?.is_visible_to(&did) {
            Ok(())
        } else {
            Err(Error::RepositoryAccess {
                repository: self.repository,
                did,
            })
        }
    }

    fn load_manifest(
        &self,
        repository: &radicle::storage::git::Repository,
    ) -> Result<Manifest, manifest::Error> {
        match &self.manifest_file {
            Some(path) => Manifest::load_file(path),
            None => Manifest::load_repository(repository),
        }
    }

    fn load_setup_manifest(
        &self,
        repository: &radicle::storage::git::Repository,
    ) -> Result<Option<Manifest>, manifest::Error> {
        match &self.manifest_file {
            Some(path) => Manifest::load_optional_file(path),
            None => Manifest::load_optional_repository(repository),
        }
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
    fn start(application: &Application) -> Result<Self, Error> {
        let home = application.member_home()?;
        let (lock, identity, node) = Self::initialize(application, &home)?;

        Ok(Self {
            home,
            _lock: lock,
            identity,
            node,
        })
    }

    fn initialize(
        application: &Application,
        home: &MemberHome,
    ) -> Result<(MemberLock, identity::Identity, NodeManager), Error> {
        let lock = MemberLock::acquire(home)?;
        let identity = application.load_identity(home)?;
        tracing::info!(
            member = %application.member,
            did = %identity.did(),
            "member identity loaded"
        );
        let node = Self::start_node(application, home, &identity)?;

        Ok((lock, identity, node))
    }

    fn start_node(
        application: &Application,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<NodeManager, Error> {
        let node = NodeManager::start_supervised(identity, home, &application.shutdown)?;
        let prepared = application
            .ensure_running()
            .and_then(|()| application.seed_repository(identity, node.node()));
        if let Err(error) = prepared {
            if let Err(stop) = Self::stop_node(node, &application.shutdown) {
                tracing::warn!(%stop, "failed to stop node after application error");
            }
            return Err(error);
        }

        Ok(node)
    }

    fn stop(self, shutdown: &Shutdown) -> Result<(), node::Error> {
        Self::stop_node(self.node, shutdown)
    }

    fn stop_node(node: NodeManager, shutdown: &Shutdown) -> Result<(), node::Error> {
        shutdown.finish_supervision(|| node.stop_supervised(shutdown))
    }
}

#[derive(Debug)]
enum SetupValidation {
    Ready(manifest::ValidatedBinding),
    ManifestMissing,
    BindingMissing,
}

impl SetupValidation {
    fn new(
        manifest: Option<Manifest>,
        member: Member,
        identity: &identity::Identity,
    ) -> Result<Self, manifest::Error> {
        let Some(manifest) = manifest else {
            return Ok(Self::ManifestMissing);
        };
        match manifest.validate(member, identity) {
            Ok(binding) => Ok(Self::Ready(binding)),
            Err(manifest::Error::Missing { .. }) => Ok(Self::BindingMissing),
            Err(error) => Err(error),
        }
    }

    fn record(self, application: &Application, home: &MemberHome) -> Result<SetupStatus, Error> {
        match self {
            Self::Ready(binding) => {
                application.record_binding(home, binding)?;
                Ok(SetupStatus::Ready {
                    generation: binding.generation(),
                })
            }
            Self::ManifestMissing => Ok(SetupStatus::ManifestMissing {
                generation: application.next_binding_generation(home)?,
            }),
            Self::BindingMissing => Ok(SetupStatus::BindingMissing {
                generation: application.next_binding_generation(home)?,
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

    #[test]
    #[cfg(not(miri))]
    fn accepts_a_ready_binding_at_the_maximum_generation() {
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
            .expect("maximum generation should be recorded");

        let status = SetupValidation::Ready(binding)
            .record(&application, &home)
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
