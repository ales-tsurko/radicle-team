//! Setup wizard orchestration.

use std::{
    collections::BTreeSet,
    io,
    io::IsTerminal as _,
    path::{Path, PathBuf},
    process::ExitStatus,
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
    provider::{self, Installed, Provider, credentials},
};

// The provider a member is to run on, beside the name it is written into the manifest under, or
// nothing when the operator left it to the agent application.
type Chosen<'a> = Option<(&'a String, &'a Provider)>;

// Pairs a name with its provider, dropping the files which could not be read.
fn readable<'a>(
    (name, provider): (&'a String, &'a Result<Provider, provider::Error>),
) -> Option<(&'a String, &'a Provider)> {
    provider.as_ref().ok().map(|provider| (name, provider))
}

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
            println!("{}", self.noninteractive_output(setup)?);
            return Ok(());
        }

        cliclack::intro(format!(" Set up {} ", self.application.member()))?;
        self.run_interactive().await
    }

    fn noninteractive_output(&self, setup: Setup) -> Result<String, Error> {
        let Some(status) = Self::pending(setup.status()) else {
            return Ok(setup.to_string());
        };
        let entry =
            ManifestEntry::new(&self.application, &setup)?.with_provider(self.named_provider()?);
        let output = match self.application.manifest_file() {
            Some(path) => Self::explicit_manifest_output(path, entry, status),
            None => Self::repository_manifest_output(entry, status),
        };

        Ok(Self::with_chat_certificate(
            output,
            self.application.member(),
            &setup,
        ))
    }

    // Refuses a `--provider` nobody installed, on the path which cannot ask.
    //
    // The same guard the interactive run applies. Without it a typo is printed in the binding the
    // operator is told to paste, accepted in the shared manifest forever because it is a plain
    // name, and the member it names has no credentials and no models.
    pub(super) fn named_provider(&self) -> Result<Option<String>, Error> {
        let Some(named) = self.application.provider() else {
            return Ok(None);
        };
        let providers = provider::installed(&provider::configuration()?)?;
        if !matches!(providers.get(named), Some(Ok(_provider))) {
            return Err(Self::unusable(&providers, named));
        }

        Ok(Some(named.to_owned()))
    }

    // The two statuses which still need a binding written. `Ready` needs none.
    const fn pending(status: SetupStatus) -> Option<SetupStatus> {
        match status {
            SetupStatus::ManifestMissing { .. } | SetupStatus::BindingMissing { .. } => {
                Some(status)
            }
            SetupStatus::Ready { .. } => None,
        }
    }

    fn with_chat_certificate(output: String, member: Member, setup: &Setup) -> String {
        format!(
            "{output}\n{} IRC certificate: {}",
            member,
            setup.chat_certificate()
        )
    }

    pub(super) fn repository_manifest_output(entry: ManifestEntry, status: SetupStatus) -> String {
        let action = match status {
            SetupStatus::ManifestMissing { .. } => {
                "add radicle-team.toml to the repository's canonical branch:\n\nversion = \
                 1\ntriggered_by = \"delegates\"\n\n[security]\nrepository = \
                 \"rad:...\"\nrecipients = [\"age1...\"]\n\n"
            }
            SetupStatus::BindingMissing { .. } => "add this binding to radicle-team.toml:\n\n",
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
        let mut decided = Decisions::default();
        loop {
            if self.step(&mut decided).await? == Step::Complete {
                return Ok(());
            }
        }
    }

    async fn step(&self, decided: &mut Decisions) -> Result<Step, Error> {
        match self.attempt().await {
            Ok(setup) => self.handle_setup(setup, decided).await,
            Err(error) => self.handle_error(error),
        }
    }

    async fn handle_setup(&self, setup: Setup, decided: &mut Decisions) -> Result<Step, Error> {
        if matches!(setup.status(), SetupStatus::Ready { .. }) {
            self.finish_setup(setup)
        } else if let Some(path) = self.application.manifest_file() {
            let update = self.manifest_update(&setup, decided).await?;
            self.prepare_explicit_manifest(path, update)
                .await
                .map(|()| Step::Retry)
        } else {
            self.prepare_repository_manifest(&setup, decided).await?;
            self.finish_setup(
                "Setup complete; awaiting manifest approval. Run the member to wait for \
                 activation.",
            )
        }
    }

    fn finish_setup(&self, message: impl std::fmt::Display) -> Result<Step, Error> {
        self.configure_sandbox()?;
        cliclack::outro(message)?;
        Ok(Step::Complete)
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
        spinner.start("Preparing member directory");
        let (progress, mut updates) = tokio::sync::watch::channel(String::new());
        let application = self.application.clone().with_setup_progress(progress);
        let setup = application.setup();
        tokio::pin!(setup);
        let result = loop {
            tokio::select! {
                result = &mut setup => break result,
                Ok(()) = updates.changed() => {
                    spinner.set_message(updates.borrow_and_update().as_str());
                }
            }
        };
        result
            .inspect(|_| spinner.stop("Repository synchronized"))
            .inspect_err(|_| spinner.error("Setup needs attention"))
    }

    async fn manifest_update(
        &self,
        setup: &Setup,
        decided: &mut Decisions,
    ) -> Result<ManifestUpdate, Error> {
        match setup.status() {
            SetupStatus::ManifestMissing { .. } => {
                self.create_manifest_update(setup, decided).await
            }
            SetupStatus::BindingMissing { .. } => self.extend_manifest_update(setup, decided).await,
            SetupStatus::Ready { .. } => Err(Error::ReadyManifest),
        }
    }

    async fn create_manifest_update(
        &self,
        setup: &Setup,
        decided: &mut Decisions,
    ) -> Result<ManifestUpdate, Error> {
        let policy = match &mut decided.policy {
            Some(policy) => policy.clone(),
            policy => policy
                .insert(self.prompt_new_policy(setup.did()).await?)
                .clone(),
        };
        let entry = self.entry(setup, decided)?;

        Ok(ManifestUpdate::create(
            entry,
            policy.security,
            policy.triggered_by,
        ))
    }

    // The binding both paths write. Which provider it names is decided the same way whether the
    // manifest is being created or extended, so it is settled in one place.
    fn entry(&self, setup: &Setup, decided: &mut Decisions) -> Result<ManifestEntry, Error> {
        Ok(ManifestEntry::new(&self.application, setup)?.with_provider(self.provider(decided)?))
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

    async fn extend_manifest_update(
        &self,
        setup: &Setup,
        decided: &mut Decisions,
    ) -> Result<ManifestUpdate, Error> {
        self.grant_security_access(setup).await?;
        let entry = self.entry(setup, decided)?;

        Ok(ManifestUpdate::extend(entry))
    }

    // Asks which model provider this member should run on, and authorizes it if it needs it.
    //
    // Leaving it unset is allowed and means the agent application decides, which is what somebody
    // setting up their first member before choosing a provider wants.
    fn provider(&self, decided: &mut Decisions) -> Result<Option<String>, Error> {
        if let Some(chosen) = &decided.provider {
            return Ok(chosen.clone());
        }

        Ok(decided.provider.insert(self.decide_provider()?).clone())
    }

    fn decide_provider(&self) -> Result<Option<String>, Error> {
        let configuration = provider::configuration()?;
        let providers = provider::installed(&configuration)?;
        let chosen = self.choice(&providers)?;
        self.authorize_chosen(&configuration, chosen)?;

        Ok(chosen.map(|(name, _provider)| name.clone()))
    }

    // The command line wins over the question, because whoever runs the member holds its
    // credentials and pays for it.
    //
    // The provider comes back beside its name, so authorizing it is not a second lookup which has
    // to invent an answer for a name that cannot be missing.
    pub(super) fn choice<'a>(&self, providers: &'a Installed) -> Result<Chosen<'a>, Error> {
        let Some(named) = self.application.provider() else {
            return Self::ask(providers);
        };

        // A name nobody installed would be written into the shared manifest and accepted there
        // forever, while the member it names has no credentials and no models. A name whose file
        // is unreadable fails on that file, which is the thing to go and fix.
        match providers.get_key_value(named) {
            Some((name, Ok(provider))) => Ok(Some((name, provider))),
            _ => Err(Self::unusable(providers, named)),
        }
    }

    // Says why a named provider cannot be used, in its own file's words where it has any. "Unknown"
    // would send the operator looking for a file which is sitting right there, broken.
    fn unusable(providers: &Installed, named: &str) -> Error {
        match providers.get(named) {
            Some(Err(error)) => provider::Error::Unusable {
                provider: named.to_owned(),
                reason: error.to_string(),
            },
            _ => provider::Error::Unknown(named.to_owned()),
        }
        .into()
    }

    // Only the providers which read. One unusable file is not a choice to offer, and it must not
    // stop the operator picking one of the others.
    fn ask(providers: &Installed) -> Result<Chosen<'_>, Error> {
        let mut select = cliclack::select("Which models should this member run on?").item(
            None,
            "decide later",
            "the agent application's own default",
        );
        for (name, provider) in providers.iter().filter_map(readable) {
            select = select.item(Some((name, provider)), name, provider.adapter());
        }

        select.interact().map_err(Error::from)
    }

    fn authorize_chosen(&self, configuration: &Path, chosen: Chosen<'_>) -> Result<(), Error> {
        let Some((name, provider)) = chosen else {
            return Ok(());
        };

        self.authorize(configuration, name, provider)
    }

    // Logs in when this provider has no credentials yet, so the member does not finish setup only
    // to fail at its first delivery.
    pub(super) fn authorize(
        &self,
        configuration: &Path,
        name: &str,
        provider: &Provider,
    ) -> Result<(), Error> {
        if !provider.needs_authorizing() {
            return Ok(());
        }
        // Read and write the machine's own store, which is where one account for one person
        // belongs. Billing a project separately is `login --home`, asked for deliberately.
        let mine = self.application.home().map(credentials::directory);
        let held = credentials::load(
            &credentials::directory(configuration),
            mine.as_deref(),
            name,
        )?;
        if held.is_some() {
            return Ok(());
        }
        let path = crate::cli::provider::login_with(provider, configuration, name, None)?;
        // Logged, not an outro: the wizard still has questions to ask, and an outro closes the
        // session out from under them.
        cliclack::log::success(format!("{name} authorized in {}", path.display()))?;

        Ok(())
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
        tokio::task::spawn_blocking(move || SecuritySetup::provision(did)).await?
    }

    async fn select_security_repository(&self, did: Did) -> Result<SecuritySetup, Error> {
        let repository: radicle::identity::RepoId =
            cliclack::input("Existing security repository RID")
                .placeholder("rad:...")
                .interact()?;
        tokio::task::spawn_blocking(move || SecuritySetup::existing(repository, did)).await?
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
        tokio::task::spawn_blocking(move || SecuritySetup::grant(repository, did)).await??;

        Ok(())
    }

    async fn prepare_repository_manifest(
        &self,
        setup: &Setup,
        decided: &mut Decisions,
    ) -> Result<(), Error> {
        let generation = setup.status().generation();
        let (checkout, preview) = self.inspect_manifest(generation).await?;
        let (checkout, (preview, published)) = if let Some(preview) = preview {
            (checkout, preview)
        } else {
            self.draft_manifest(checkout, setup, decided).await?
        };
        Self::confirm_patch(preview, published)?;
        self.publish_manifest(checkout, setup.clone()).await
    }

    async fn inspect_manifest(
        &self,
        generation: u64,
    ) -> Result<(RepositoryCheckout, Option<(String, bool)>), Error> {
        let checkout = RepositoryCheckout::open(&self.application).await?;
        tokio::task::spawn_blocking(move || {
            let preview = checkout.preview(generation)?;
            Ok::<_, Error>((checkout, preview))
        })
        .await?
    }

    async fn draft_manifest(
        &self,
        checkout: RepositoryCheckout,
        setup: &Setup,
        decided: &mut Decisions,
    ) -> Result<(RepositoryCheckout, (String, bool)), Error> {
        let update = self.manifest_update(setup, decided).await?;
        self.show_manifest(update.entry())?;
        let generation = setup.status().generation();
        tokio::task::spawn_blocking(move || {
            checkout.prepare(update)?;
            let preview = checkout
                .preview(generation)?
                .ok_or(Error::MissingProposal)?;
            Ok((checkout, preview))
        })
        .await?
    }

    fn confirm_patch(preview: String, published: bool) -> Result<(), Error> {
        if !published {
            cliclack::note("Proposed manifest", preview)?;
            if !cliclack::confirm(
                "Publish this manifest as a Radicle patch under the member identity?",
            )
            .initial_value(true)
            .interact()?
            {
                return Err(Error::Cancelled);
            }
        }
        Ok(())
    }

    async fn prepare_explicit_manifest(
        &self,
        path: &Path,
        update: ManifestUpdate,
    ) -> Result<(), Error> {
        self.show_manifest(update.entry())?;
        if self.confirm_explicit_manifest(path)? {
            Self::update_explicit_manifest(path, update).await
        } else {
            Err(Error::Cancelled)
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
        let changed = tokio::task::spawn_blocking(move || update.prepare(&path)).await??;
        if changed {
            cliclack::log::success("Explicit manifest updated")?;
        }
        Ok(())
    }

    fn show_manifest(&self, entry: &ManifestEntry) -> Result<(), Error> {
        cliclack::note("Member identity", entry.did)?;
        cliclack::note("Required manifest entry", entry).map_err(Error::from)
    }

    async fn publish_manifest(
        &self,
        checkout: RepositoryCheckout,
        setup: Setup,
    ) -> Result<(), Error> {
        let patch =
            tokio::task::spawn_blocking(move || checkout.publish(setup.status().generation()))
                .await??;
        // Publication releases the home lock before starting the member's node to announce it.
        cliclack::log::success(format!("Manifest patch: {patch}"))?;
        self.application.announce_setup_patch().await?;
        Ok(())
    }

    fn retry_access(&self, error: crate::Error) -> Result<(), Error> {
        self.show_access_error(&error)?;
        self.confirm_retry(error)
    }

    fn show_access_error(&self, error: &crate::Error) -> Result<(), Error> {
        cliclack::log::warning(error.to_string())?;
        if let Some(did) = Self::error_did(error) {
            let mut message = did.to_string();
            if matches!(error, crate::Error::RepositoryAccess { .. }) {
                message.push_str("\n\nGrant this DID access to the private repository.");
            }
            cliclack::note("Member identity", message)?;
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
            crate::Error::Sync { .. }
                | crate::Error::SyncTimeout { .. }
                | crate::Error::RepositoryAccess { .. }
                | crate::Error::Node(crate::node::Error::SeedsUnavailable { .. })
        )
    }

    const fn error_did(error: &crate::Error) -> Option<Did> {
        match error {
            crate::Error::Sync { did, .. }
            | crate::Error::SyncTimeout { did, .. }
            | crate::Error::RepositoryAccess { did, .. } => Some(*did),
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

impl From<crate::cli::provider::Error> for Error {
    fn from(error: crate::cli::provider::Error) -> Self {
        Self::Provider(Box::new(error))
    }
}

impl From<crate::provider::Error> for Error {
    fn from(error: crate::provider::Error) -> Self {
        crate::cli::provider::Error::from(error).into()
    }
}

impl From<crate::provider::CredentialsError> for Error {
    fn from(error: crate::provider::CredentialsError) -> Self {
        crate::cli::provider::Error::from(error).into()
    }
}

// What the operator has already been asked, so a retry does not ask it again.
//
// Synchronization failures can be retried without asking the same questions again.
#[derive(Debug, Default)]
struct Decisions {
    policy: Option<NewManifestPolicy>,
    provider: Option<Option<String>>,
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
    /// Choosing or authorizing a model provider failed.
    ///
    /// One variant rather than three: the same failure reaches here through the listing and
    /// through the login, and two spellings of it is a match nobody can write correctly.
    #[error(transparent)]
    Provider(Box<crate::cli::provider::Error>),
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
    /// The operator's personal Radicle profile could not be loaded.
    #[error("could not load your Radicle identity: {0}")]
    OperatorProfile(radicle::profile::Error),
    /// The operator's signing key is unavailable after authentication.
    #[error(
        "could not unlock your Radicle signing key: {0}; ensure ssh-agent is running and run `rad \
         auth`"
    )]
    OperatorSigner(radicle::profile::SignerError),
    /// Creating the member's working copies failed.
    #[error(transparent)]
    Workspace(#[from] crate::WorkspaceError),
    /// Opening or validating the member's Radicle repository failed.
    #[error("member repository operation failed: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    /// Reading the repository's collaborative objects failed.
    #[error("member collaborative object operation failed: {0}")]
    Cob(#[from] radicle::cob::store::Error),
    /// Creating the manifest patch failed.
    #[error("manifest patch creation failed: {0}")]
    Patch(#[from] radicle::cob::patch::Error),
    /// Opening the member's patch cache failed.
    #[error("member profile operation failed: {0}")]
    Profile(#[from] radicle::profile::Error),
    /// The member's signing key is unavailable.
    #[error("could not load the member's signing key: {0}")]
    Signer(#[from] radicle::profile::SignerError),
    /// The generated patch title is invalid.
    #[error("invalid manifest patch title: {0}")]
    PatchTitle(#[from] radicle::cob::TitleError),
    /// A Radicle command returned a malformed repository identifier.
    #[error("Radicle returned an invalid repository ID: {0}")]
    CheckoutIdentity(radicle::identity::IdError),
    /// A local Git operation failed.
    #[error("local Git operation failed: {0}")]
    Git(#[from] git2::Error),
    /// The coder checkout is outside the expected member directory.
    #[error("setup checkout is outside the member home")]
    CheckoutLocation,
    /// The coder checkout does not share the member's Git object store.
    #[error("coder worktree does not use the member's shared Git store")]
    CheckoutStorage,
    /// The coder checkout's Radicle remote points to another repository.
    #[error("setup checkout belongs to another repository")]
    CheckoutRepository,
    /// Git cannot address a checkout or storage path as UTF-8.
    #[error("path is not UTF-8: '{}'", .0.display())]
    NonUtf8Path(PathBuf),
    /// The checkout's proposal does not descend from canonical history.
    #[error(
        "setup checkout contains commits outside the canonical history; review {} before retrying",
        .0.display()
    )]
    NonCanonicalCheckout(PathBuf),
    /// Preparing a proposal would overwrite uncommitted work.
    #[error("coder worktree has uncommitted changes; review {} before retrying", .0.display())]
    DirtyCheckout(PathBuf),
    /// The canonical head changed after the checkout was prepared.
    #[error("canonical branch changed during setup; retry setup")]
    CanonicalChanged,
    /// The proposed binding uses a different identity generation.
    #[error("setup manifest binding generation is {actual}, expected {expected}")]
    BindingGeneration {
        /// Generation required by the provisioned identity.
        expected: u64,
        /// Generation found in the proposed manifest.
        actual: u64,
    },
    /// A proposed manifest is a symlink or another nonregular file.
    #[error("setup manifest must be a regular file: '{}'", .0.display())]
    IrregularManifest(PathBuf),
    /// The working manifest differs from the committed proposal.
    #[error("setup manifest has changed since its commit; review it before retrying")]
    ChangedManifest,
    /// The working copy deletes the manifest instead of proposing a binding.
    #[error("setup manifest was removed; restore it before retrying")]
    RemovedManifest,
    /// A proposal commit has an unsupported number of parents.
    #[error("setup proposal must contain a single manifest commit")]
    InvalidProposalHistory,
    /// The proposal commit includes changes outside the manifest.
    #[error("setup commit includes changes outside the manifest")]
    NonManifestChanges,
    /// Publication was requested without a proposed manifest change.
    #[error("coder worktree has no proposed manifest change")]
    MissingProposal,
    /// Publication would include unrelated staged work.
    #[error("setup checkout has unrelated staged changes")]
    StagedChanges,
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
    Task(#[from] tokio::task::JoinError),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Application(Box::new(error))
    }
}
