//! Library for running an AI contributor on Radicle.

use std::path::{Path, PathBuf};

use radicle::{
    identity::{Did, RepoId},
    storage::{ReadRepository as _, ReadStorage as _},
};

use crate::{
    config::Config,
    home::MemberHome,
    identity::{GenerationStore, Manager, MemberLock, Provider},
    member::Member,
};

/// Command-line interface for selecting and running a team member.
pub mod cli;
/// Repository-wide project configuration.
pub mod config;
/// Filesystem locations used by each team member.
pub mod home;
/// Radicle identity ownership and key management.
pub mod identity;
/// Fixed team members and their profiles.
pub mod member;
/// Read-only Radicle activity watching.
pub mod watcher;

/// A single Radicle team member application.
#[derive(Debug)]
pub struct Application {
    member: Member,
    repository: RepoId,
    home: Option<PathBuf>,
    identity_provider: Provider,
    config_file: Option<PathBuf>,
}

impl Application {
    /// Creates an application for `member` watching `repository`.
    #[must_use]
    pub const fn new(member: Member, repository: RepoId) -> Self {
        Self {
            member,
            repository,
            home: None,
            identity_provider: Provider::Managed {
                protection: identity::KeyProtection::SystemCredential,
            },
            config_file: None,
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

    /// Uses an explicit project configuration file instead of the repository copy.
    #[must_use]
    pub fn with_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
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

    /// Returns the explicit project configuration file, if configured.
    #[must_use]
    pub fn config_file(&self) -> Option<&Path> {
        self.config_file.as_deref()
    }

    /// Runs the team member.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration, identity, repository, node, event stream, or local state
    /// validation fails.
    pub fn run(&self) -> Result<(), Error> {
        let home = self.member_home()?;
        let _lock = MemberLock::acquire(&home)?;
        let identity = self.load_identity(&home)?;
        tracing::info!(member = %self.member, did = %identity.did(), "member identity loaded");
        let binding = self.validate_binding(&home, &identity)?;
        let profile = self.member.profile();
        tracing::info!(
            member = profile.name,
            role = profile.role,
            generation = binding.generation(),
            "team member initialized"
        );
        watcher::Watcher::new(self.member, self.repository)
            .run(identity.profile(), &home.state_database(), |activity| {
                tracing::info!(?activity, "activity received");
            })
            .map_err(Error::from)
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

    fn validate_binding(
        &self,
        home: &MemberHome,
        identity: &identity::Identity,
    ) -> Result<config::ValidatedBinding, Error> {
        let binding = self.load_validated_binding(identity)?;
        self.record_binding(home, binding)?;

        Ok(binding)
    }

    fn load_validated_binding(
        &self,
        identity: &identity::Identity,
    ) -> Result<config::ValidatedBinding, Error> {
        let repository = identity.profile().storage.repository(self.repository)?;
        self.validate_repository_access(&repository, identity.did())?;
        let config = self.load_config(&repository)?;

        config.validate(self.member, identity).map_err(Error::from)
    }

    fn record_binding(
        &self,
        home: &MemberHome,
        binding: config::ValidatedBinding,
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

    fn load_config(
        &self,
        repository: &radicle::storage::git::Repository,
    ) -> Result<Config, config::Error> {
        match &self.config_file {
            Some(path) => Config::load_file(path),
            None => Config::load_repository(repository),
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
    /// Loading or validating project configuration failed.
    #[error("project configuration error: {0}")]
    Configuration(#[from] config::Error),
    /// Recording the binding generation failed.
    #[error("member binding generation error: {0}")]
    Generation(#[from] identity::GenerationError),
    /// Watching Radicle activity failed.
    #[error("Radicle watcher error: {0}")]
    Watcher(#[from] watcher::Error),
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use radicle::identity::RepoId;

    use crate::{Application, member::Member};

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
        assert_eq!(application.config_file(), None);
    }

    #[test]
    fn application_accepts_explicit_home() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let application = Application::new(Member::Vera, repository).with_home("/team");

        assert_eq!(application.home(), Some(std::path::Path::new("/team")));
    }

    #[test]
    fn application_accepts_identity_and_project_configuration() {
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let provider = crate::identity::Provider::RadicleHome {
            home: "/radicle".into(),
            expected_did: None,
        };
        let application = Application::new(Member::Vera, repository)
            .with_identity_provider(provider.clone())
            .with_config_file("/project.toml");

        assert_eq!(application.identity_provider(), &provider);
        assert_eq!(
            application.config_file(),
            Some(std::path::Path::new("/project.toml"))
        );
    }
}
