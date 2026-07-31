//! Setup wizard orchestration.

use std::{
    collections::BTreeSet, io, io::IsTerminal as _, path::Path, process::ExitStatus,
    str::FromStr as _,
};

use radicle::identity::Did;

use crate::{
    Application, Setup, SetupStatus,
    cli::setup::{
        ManifestEntry, ManifestUpdate, RepositoryCheckout, SandboxSetup, SecurityManifest,
        SecuritySetup,
    },
    manifest::{FILE_NAME, TriggerAccess},
    member::Member,
};

/// Guides a user through identity and repository setup.
#[derive(Debug)]
pub(crate) struct SetupWizard {
    application: Application,
}

impl SetupWizard {
    pub(crate) const fn new(application: Application) -> Self {
        Self { application }
    }

    pub(crate) async fn run(self) -> Result<(), Error> {
        if !Self::is_interactive() {
            let setup = self.application.setup().await?;
            self.initialize_sandbox()?;
            println!("{}", self.noninteractive_output(setup));
            return Ok(());
        }

        cliclack::intro(format!(" Set up {} ", self.application.member()))?;
        self.run_interactive().await
    }

    fn noninteractive_output(&self, setup: Setup) -> String {
        match (self.application.manifest_file(), setup.status()) {
            (
                Some(path),
                status @ (SetupStatus::ManifestMissing { .. } | SetupStatus::BindingMissing { .. }),
            ) => {
                let entry = ManifestEntry::new(&self.application, &setup);
                Self::explicit_manifest_output(path, entry, status)
            }
            (
                None,
                status @ (SetupStatus::ManifestMissing { .. } | SetupStatus::BindingMissing { .. }),
            ) => Self::repository_manifest_output(
                ManifestEntry::new(&self.application, &setup),
                status,
            ),
            _ => setup.to_string(),
        }
    }

    pub(super) fn repository_manifest_output(entry: ManifestEntry, status: SetupStatus) -> String {
        let action = match status {
            SetupStatus::ManifestMissing { .. } => {
                "add .radicle-team.toml to the repository's canonical branch:\n\nversion = \
                 1\ntriggered_by = \"delegates\"\n\n[security]\nrepository = \
                 \"rad:...\"\nrecipients = [\"age1...\"]\n\n"
            }
            SetupStatus::BindingMissing { .. } => "add this binding to .radicle-team.toml:\n\n",
            SetupStatus::Ready { .. } => {
                return format!("{} identity: {}", entry.member, entry.did);
            }
        };
        format!(
            "{member} identity: {did}\n{action}{entry}",
            member = entry.member,
            did = entry.did,
        )
    }

    pub(super) fn explicit_manifest_output(
        path: &Path,
        entry: ManifestEntry,
        status: SetupStatus,
    ) -> String {
        let action = match status {
            SetupStatus::ManifestMissing { .. } => {
                format!(
                    "create explicit manifest '{}':\n\nversion = 1\ntriggered_by = \
                     \"delegates\"\n\n[security]\nrepository = \"rad:...\"\nrecipients = \
                     [\"age1...\"]\n\n",
                    path.display()
                )
            }
            SetupStatus::BindingMissing { .. } => {
                format!(
                    "add this binding to explicit manifest '{}':\n\n",
                    path.display()
                )
            }
            SetupStatus::Ready { .. } => {
                return format!("{} identity: {}", entry.member, entry.did);
            }
        };
        format!(
            "{member} identity: {did}\n{action}{entry}",
            member = entry.member,
            did = entry.did,
        )
    }

    async fn run_interactive(&self) -> Result<(), Error> {
        let mut policy = None;
        loop {
            if self.step(&mut policy).await? == Step::Complete {
                return Ok(());
            }
        }
    }

    async fn step(&self, policy: &mut Option<NewManifestPolicy>) -> Result<Step, Error> {
        match self.attempt().await {
            Ok(setup) => self.handle_setup(setup, policy).await,
            Err(error) => self.handle_error(error),
        }
    }

    async fn handle_setup(
        &self,
        setup: Setup,
        policy: &mut Option<NewManifestPolicy>,
    ) -> Result<Step, Error> {
        if matches!(setup.status(), SetupStatus::Ready { .. }) {
            self.configure_sandbox()?;
            cliclack::outro(setup)?;
            Ok(Step::Complete)
        } else {
            self.prepare_manifest(setup, policy).await?;
            Ok(Step::Retry)
        }
    }

    fn handle_error(&self, error: crate::Error) -> Result<Step, Error> {
        if Self::can_retry(&error) {
            self.retry_access(error)?;
            Ok(Step::Retry)
        } else {
            Err(error.into())
        }
    }

    async fn attempt(&self) -> Result<Setup, crate::Error> {
        let spinner = cliclack::spinner();
        spinner.start("Preparing identity and synchronizing repository");
        self.application
            .setup()
            .await
            .inspect(|_| spinner.stop("Repository synchronized"))
            .inspect_err(|_| spinner.error("Setup needs attention"))
    }

    async fn prepare_manifest(
        &self,
        setup: Setup,
        policy: &mut Option<NewManifestPolicy>,
    ) -> Result<(), Error> {
        let update = self.manifest_update(&setup, policy).await?;
        self.show_manifest(update.entry())?;
        if let Some(path) = self.application.manifest_file() {
            return self.prepare_explicit_manifest(path, update).await;
        }

        self.prepare_repository_manifest(update).await
    }

    async fn manifest_update(
        &self,
        setup: &Setup,
        policy: &mut Option<NewManifestPolicy>,
    ) -> Result<ManifestUpdate, Error> {
        match setup.status() {
            SetupStatus::ManifestMissing { .. } => self.create_manifest_update(setup, policy).await,
            SetupStatus::BindingMissing { .. } => self.extend_manifest_update(setup).await,
            SetupStatus::Ready { .. } => Err(Error::ReadyManifest),
        }
    }

    async fn create_manifest_update(
        &self,
        setup: &Setup,
        policy: &mut Option<NewManifestPolicy>,
    ) -> Result<ManifestUpdate, Error> {
        let policy = match policy {
            Some(policy) => policy.clone(),
            None => policy
                .insert(self.prompt_new_policy(setup.did()).await?)
                .clone(),
        };
        let entry = ManifestEntry::new(&self.application, setup);

        Ok(ManifestUpdate::create(
            entry,
            policy.security,
            policy.triggered_by,
        ))
    }

    async fn prompt_new_policy(&self, did: Did) -> Result<NewManifestPolicy, Error> {
        let triggered_by = Self::trigger_access()?;
        let security = self.provision_security(did).await?;

        Ok(NewManifestPolicy {
            security,
            triggered_by,
        })
    }

    fn trigger_access() -> Result<TriggerAccess, Error> {
        let choice = cliclack::select("Who may trigger this repository's agents?")
            .initial_value(TriggerChoice::Delegates)
            .item(
                TriggerChoice::Delegates,
                "Repository delegates",
                "recommended",
            )
            .item(TriggerChoice::Dids, "Delegates and selected identities", "")
            .item(
                TriggerChoice::Anyone,
                "Anyone",
                "only for a private repository",
            )
            .interact()?;
        match choice {
            TriggerChoice::Delegates => Ok(TriggerAccess::Delegates),
            TriggerChoice::Anyone => Ok(TriggerAccess::Anyone),
            TriggerChoice::Dids => Self::trigger_dids(),
        }
    }

    fn trigger_dids() -> Result<TriggerAccess, Error> {
        let value: String = cliclack::input("Allowed identities (one DID per line)")
            .placeholder("did:key:z6Mk...")
            .multiline()
            .interact()?;
        let dids = value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(Did::from_str)
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(Error::TriggerDid)?;
        if dids.is_empty() {
            return Err(Error::EmptyTriggerDids);
        }

        Ok(TriggerAccess::Dids(dids))
    }

    async fn extend_manifest_update(&self, setup: &Setup) -> Result<ManifestUpdate, Error> {
        self.grant_security_access(setup).await?;
        let entry = ManifestEntry::new(&self.application, setup);

        Ok(ManifestUpdate::extend(entry))
    }

    async fn provision_security(&self, did: Did) -> Result<SecurityManifest, Error> {
        let choice = Self::security_repository_choice()?;
        let setup = self.prepare_security_repository(choice, did).await?;
        self.show_security_recovery_key(&setup)?;

        Ok(setup.manifest())
    }

    fn security_repository_choice() -> Result<SecurityRepositoryChoice, Error> {
        cliclack::select("Configure the private security-report repository")
            .initial_value(SecurityRepositoryChoice::Create)
            .item(
                SecurityRepositoryChoice::Create,
                "Create a repository",
                "recommended",
            )
            .item(
                SecurityRepositoryChoice::Existing,
                "Use an existing repository",
                "",
            )
            .interact()
            .map_err(Error::from)
    }

    async fn prepare_security_repository(
        &self,
        choice: SecurityRepositoryChoice,
        did: Did,
    ) -> Result<SecuritySetup, Error> {
        match choice {
            SecurityRepositoryChoice::Create => self.provision_security_repository(did).await,
            SecurityRepositoryChoice::Existing => self.select_security_repository(did).await,
        }
    }

    async fn provision_security_repository(&self, did: Did) -> Result<SecuritySetup, Error> {
        tokio::task::spawn_blocking(move || SecuritySetup::provision(did))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    async fn select_security_repository(&self, did: Did) -> Result<SecuritySetup, Error> {
        let repository: radicle::identity::RepoId =
            cliclack::input("Existing security repository RID")
                .placeholder("rad:...")
                .interact()?;
        tokio::task::spawn_blocking(move || SecuritySetup::existing(repository, did))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    fn show_security_recovery_key(&self, setup: &SecuritySetup) -> Result<(), Error> {
        cliclack::note(
            "Security recovery key",
            format!(
                "{}\n\nSave this key outside the agent environment. It is shown only once.",
                setup.recovery_key()
            ),
        )
        .map_err(Error::from)
    }

    async fn grant_security_access(&self, setup: &Setup) -> Result<(), Error> {
        let Some(policy) = setup.security() else {
            return Ok(());
        };
        let repository = policy.repository();
        let did = setup.did();
        let granted = cliclack::confirm("Grant this member access to the security repository?")
            .initial_value(true)
            .interact()?;
        if !granted {
            return Err(Error::Cancelled);
        }
        tokio::task::spawn_blocking(move || SecuritySetup::grant(repository, did))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;

        Ok(())
    }

    async fn prepare_repository_manifest(&self, update: ManifestUpdate) -> Result<(), Error> {
        let repository = self.application.repository();
        let checkout = tokio::task::spawn_blocking(move || RepositoryCheckout::current(repository))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        match checkout {
            Some(checkout) => self.prepare_checkout(checkout, update).await,
            None => self.missing_checkout(),
        }
    }

    async fn prepare_explicit_manifest(
        &self,
        path: &Path,
        update: ManifestUpdate,
    ) -> Result<(), Error> {
        if self.confirm_explicit_manifest(path)? {
            Self::update_explicit_manifest(path, update).await
        } else {
            self.wait_for_manifest()
        }
    }

    fn confirm_explicit_manifest(&self, path: &Path) -> Result<bool, Error> {
        cliclack::note("Explicit manifest", path.display())?;
        cliclack::confirm("Update the explicitly selected manifest?")
            .initial_value(true)
            .interact()
            .map_err(Error::from)
    }

    async fn update_explicit_manifest(path: &Path, update: ManifestUpdate) -> Result<(), Error> {
        let path = path.to_owned();
        let changed = tokio::task::spawn_blocking(move || update.prepare(&path))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        if changed {
            cliclack::log::success("Explicit manifest updated")?;
        }
        Ok(())
    }

    fn show_manifest(&self, entry: &ManifestEntry) -> Result<(), Error> {
        cliclack::note("Member identity", entry.did)?;
        cliclack::note("Required manifest entry", entry).map_err(Error::from)
    }

    async fn prepare_checkout(
        &self,
        checkout: RepositoryCheckout,
        update: ManifestUpdate,
    ) -> Result<(), Error> {
        if self.confirm_publish(&checkout).await? {
            Self::publish_manifest(checkout, update).await?;
        }
        self.wait_for_manifest()
    }

    async fn confirm_publish(&self, checkout: &RepositoryCheckout) -> Result<bool, Error> {
        let checkout = checkout.clone();
        let member = self.application.member();
        let commands = tokio::task::spawn_blocking(move || checkout.commands(member))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        cliclack::note("Commands", commands)?;
        cliclack::confirm("Create the manifest commit and open a Radicle patch?")
            .initial_value(true)
            .interact()
            .map_err(Error::from)
    }

    async fn publish_manifest(
        checkout: RepositoryCheckout,
        update: ManifestUpdate,
    ) -> Result<(), Error> {
        let published = tokio::task::spawn_blocking(move || checkout.publish(update))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        if published {
            cliclack::log::success("Manifest patch published")?;
        }

        Ok(())
    }

    fn missing_checkout(&self) -> Result<(), Error> {
        cliclack::log::warning("The current directory is not this repository's Radicle checkout.")?;
        self.wait_for_manifest()
    }

    fn wait_for_manifest(&self) -> Result<(), Error> {
        let ready = cliclack::confirm(
            "Continue after the manifest change has reached the canonical branch?",
        )
        .initial_value(true)
        .interact()?;
        if ready { Ok(()) } else { Err(Error::Cancelled) }
    }

    fn retry_access(&self, error: crate::Error) -> Result<(), Error> {
        self.show_access_error(&error)?;
        self.confirm_retry(error)
    }

    fn show_access_error(&self, error: &crate::Error) -> Result<(), Error> {
        cliclack::log::warning(error.to_string())?;
        if let Some(did) = Self::error_did(error) {
            cliclack::note(
                "Member identity",
                format!("{did}\n\nFor a private repository, grant this DID access."),
            )?;
        }

        Ok(())
    }

    fn confirm_retry(&self, error: crate::Error) -> Result<(), Error> {
        if cliclack::confirm("Retry repository synchronization?")
            .initial_value(true)
            .interact()?
        {
            Ok(())
        } else {
            Err(Error::from(error))
        }
    }

    const fn can_retry(error: &crate::Error) -> bool {
        matches!(
            error,
            crate::Error::Sync { .. } | crate::Error::RepositoryAccess { .. }
        )
    }

    const fn error_did(error: &crate::Error) -> Option<Did> {
        match error {
            crate::Error::Sync { did, .. } | crate::Error::RepositoryAccess { did, .. } => {
                Some(*did)
            }
            _ => None,
        }
    }

    fn is_interactive() -> bool {
        io::stdin().is_terminal() && io::stdout().is_terminal()
    }

    fn initialize_sandbox(&self) -> Result<(), Error> {
        SandboxSetup::load(self.application.sandbox_config_path()?)?.initialize()
    }

    fn configure_sandbox(&self) -> Result<(), Error> {
        SandboxSetup::load(self.application.sandbox_config_path()?)?.configure()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Retry,
    Complete,
}

/// Repository-wide policy chosen once when the wizard creates a manifest.
#[derive(Debug, Clone)]
struct NewManifestPolicy {
    security: SecurityManifest,
    triggered_by: TriggerAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerChoice {
    Delegates,
    Dids,
    Anyone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurityRepositoryChoice {
    Create,
    Existing,
}

/// Error running interactive setup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Application setup failed.
    #[error(transparent)]
    Application(Box<crate::Error>),
    /// The proposed repository manifest is invalid.
    #[error(transparent)]
    Manifest(#[from] crate::manifest::Error),
    /// The working manifest is not valid editable TOML.
    #[error("invalid editable repository manifest: {0}")]
    Toml(#[from] toml_edit::TomlError),
    /// A new manifest could not be serialized as TOML.
    #[error("failed to serialize repository manifest: {0}")]
    TomlSerialization(#[from] toml_edit::ser::Error),
    /// Local sandbox configuration is unsafe or invalid.
    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::ConfigError),
    /// A binding generation cannot be represented as a TOML integer.
    #[error("binding generation is too large: {0}")]
    Generation(#[from] std::num::TryFromIntError),
    /// The user cancelled setup.
    #[error("setup was cancelled")]
    Cancelled,
    /// The operating-system home directory is unavailable for `~` expansion.
    #[error("could not expand '~' because the operating-system home directory is unavailable")]
    HomeUnavailable,
    /// A domain allowlist was selected without any domains.
    #[error("at least one sandbox network domain is required")]
    EmptySandboxDomains,
    /// A trigger allowlist was selected without any identities.
    #[error("at least one triggering identity is required")]
    EmptyTriggerDids,
    /// A triggering identity is not a valid DID.
    #[error("invalid triggering identity: {0}")]
    TriggerDid(radicle::identity::did::DidError),
    /// A setup filesystem or subprocess operation failed.
    #[error("setup I/O failed: {0}")]
    Io(#[from] io::Error),
    /// `rad inspect` returned malformed output.
    #[error("the current Radicle checkout returned an invalid repository ID: {0}")]
    CheckoutIdentity(radicle::identity::IdError),
    /// The selected Git repository has no working tree.
    #[error("the current Radicle repository is bare and cannot be used as a checkout")]
    BareCheckout,
    /// A local Git repository operation failed.
    #[error("local Git operation failed: {0}")]
    Git(#[from] git2::Error),
    /// The working manifest has changes that the wizard must not overwrite.
    #[error("'{FILE_NAME}' has uncommitted changes; review them before continuing")]
    DirtyManifest,
    /// Git ignores the repository manifest, so it cannot be published.
    #[error("'{FILE_NAME}' is ignored by Git; remove its ignore rule before continuing")]
    IgnoredManifest,
    /// The index contains changes that must not be included in the generated commit.
    #[error("the Git index has staged changes; commit or unstage them before continuing")]
    StagedChanges,
    /// The current branch already belongs to another Radicle patch.
    #[error("the current branch already tracks a Radicle patch; switch to the canonical branch")]
    ExistingPatch,
    /// The current checkout is not exactly at its canonical Radicle upstream.
    #[error(
        "the current branch is not at the canonical Radicle head; switch to an up-to-date \
         canonical branch"
    )]
    NonCanonicalCheckout,
    /// The working manifest binds the member differently.
    #[error("'{FILE_NAME}' already contains a different binding for {0}")]
    ConflictingBinding(Member),
    /// The manifest's members value is not an editable table.
    #[error("'{FILE_NAME}' has an unsupported members representation")]
    InvalidMembers,
    /// A binding cannot be appended because the working manifest is absent.
    #[error("'{FILE_NAME}' is missing from the current checkout")]
    MissingManifest,
    /// A manifest appeared while the wizard was preparing to create it.
    #[error("the repository manifest appeared during setup; rerun setup")]
    ExistingManifest,
    /// An existing security-report repository must not be public.
    #[error("security-report repository {0} is public; select a private repository")]
    PublicSecurityRepository(radicle::identity::RepoId),
    /// The wizard was asked to modify an already valid manifest.
    #[error("the repository manifest is already ready")]
    ReadyManifest,
    /// The checkout changed after the generated commit and cannot be restored safely.
    #[error("the checkout changed while publishing the generated manifest commit")]
    CheckoutChangedDuringPublish,
    /// Publishing and restoring the generated change both failed.
    #[error(
        "manifest patch operation failed ({operation}); restoring the checkout also failed \
         ({recovery})"
    )]
    PatchRecovery {
        /// Original add, commit, or push failure.
        operation: Box<Error>,
        /// Checkout recovery failure.
        recovery: Box<Error>,
    },
    /// A Git command failed.
    #[error("command '{command}' exited with {status}")]
    Command {
        /// Display form of the failed command.
        command: String,
        /// Process exit status.
        status: ExitStatus,
    },
    /// A blocking setup task failed.
    #[error("setup task failed: {0}")]
    Task(String),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Application(Box::new(error))
    }
}
