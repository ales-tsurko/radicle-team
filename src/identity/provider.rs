//! Identity provisioning and signer loading.
//!
//! [`Manager`] is the identity subsystem's entry point. It either initializes a native Radicle
//! profile below the selected member home or loads a dedicated existing Radicle home. In both cases
//! it verifies the private key, derives the DID, checks an optional expected DID, and returns an
//! [`Identity`] that owns the validated Radicle profile and any signer unlocked by its stored
//! passphrase.
//!
//! Managed encrypted keys obtain their passphrase from `RAD_PASSPHRASE` or the configured
//! credential store. The resulting identity deliberately exposes command configuration methods
//! instead of its secret: trusted Radicle subprocesses receive the required environment, while
//! model subprocesses have identity-bearing environment variables removed.
//!
//! The application invokes this module after acquiring the member-home lock and before validating
//! repository manifest or starting the watcher.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr as _,
};

use radicle::{
    crypto::{
        Seed,
        ssh::{
            Keystore, Passphrase,
            keystore::{MemorySigner, MemorySignerError},
        },
    },
    git,
    identity::Did,
    node::Alias,
    profile::{Config, Home as RadicleHome, Profile as RadicleProfile},
    storage::git::{Storage, transport},
};
use rand::distr::{Alphanumeric, SampleString as _};

use crate::{
    home::MemberHome,
    identity::{
        credentials::{CredentialId, CredentialStore, SystemCredentialStore},
        permissions::Permissions,
    },
    member::Member,
};

const PASSPHRASE_LENGTH: usize = 48;

/// A validated Radicle signing identity and profile.
pub struct Identity {
    profile: RadicleProfile,
    did: Did,
    passphrase: Option<Passphrase>,
    signer: Option<MemorySigner>,
}

impl fmt::Debug for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Identity")
            .field("home", &self.profile.home().path())
            .field("did", &self.did)
            .field("encrypted", &self.passphrase.is_some())
            .finish()
    }
}

impl Identity {
    /// Returns the identity's Radicle DID.
    #[must_use]
    pub const fn did(&self) -> Did {
        self.did
    }

    /// Returns the loaded Radicle profile.
    #[must_use]
    pub const fn profile(&self) -> &RadicleProfile {
        &self.profile
    }

    pub(crate) fn memory_signer(&self) -> Option<MemorySigner> {
        self.signer.clone()
    }

    /// Configures a trusted Radicle subprocess with this identity.
    pub fn configure_radicle_command(&self, command: &mut Command) {
        command
            .env(radicle::profile::env::RAD_HOME, self.profile.home().path())
            .env_remove(radicle::profile::env::RAD_SOCKET);
        match &self.passphrase {
            Some(passphrase) => {
                command.env(radicle::profile::env::RAD_PASSPHRASE, passphrase);
            }
            None => {
                command.env_remove(radicle::profile::env::RAD_PASSPHRASE);
            }
        }
    }

    /// Removes identity credentials from an untrusted subprocess environment.
    pub fn sanitize_command(command: &mut Command) {
        command
            .env_remove(radicle::profile::env::RAD_PASSPHRASE)
            .env_remove(radicle::profile::env::RAD_HOME)
            .env_remove(radicle::profile::env::RAD_SOCKET)
            .env_remove("SSH_AUTH_SOCK");
    }

    fn from_profile(
        profile: RadicleProfile,
        passphrase: Option<Passphrase>,
    ) -> Result<Self, Error> {
        let encrypted = profile.keystore.is_encrypted()?;
        let signer = if encrypted && passphrase.is_none() {
            drop(profile.signer()?);
            None
        } else {
            Some(MemorySigner::load(&profile.keystore, passphrase.clone())?)
        };
        Permissions::private_file(profile.keystore.secret_key_path())?;
        let did = Did::from(profile.id());

        Ok(Self {
            profile,
            did,
            passphrase,
            signer,
        })
    }
}

/// Loads and provisions member identities.
#[derive(Debug)]
pub struct Manager<Credentials = SystemCredentialStore> {
    credentials: Credentials,
}

impl Default for Manager<SystemCredentialStore> {
    fn default() -> Self {
        Self {
            credentials: SystemCredentialStore,
        }
    }
}

impl<Credentials> Manager<Credentials>
where
    Credentials: CredentialStore,
{
    /// Creates an identity manager with an explicit credential store.
    #[must_use]
    pub const fn new(credentials: Credentials) -> Self {
        Self { credentials }
    }

    /// Loads or provisions the selected member identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile, private key, passphrase, permissions,
    /// or expected DID cannot be validated.
    pub fn load(
        &self,
        member: Member,
        member_home: &MemberHome,
        provider: &Provider,
    ) -> Result<Identity, Error> {
        match provider {
            Provider::Managed { protection } => self.load_managed(member, member_home, *protection),
            Provider::RadicleHome { home, expected_did } => {
                self.load_existing(member, home, *expected_did)
            }
        }
    }

    fn load_managed(
        &self,
        member: Member,
        member_home: &MemberHome,
        protection: KeyProtection,
    ) -> Result<Identity, Error> {
        Permissions::secure_directory(member_home.path())?;
        let home = RadicleHome::new(member_home.radicle())?;
        let keystore = Keystore::new(&home.keys());
        let initialized = Self::profile_files(&home, &keystore);

        let (profile, passphrase) = match initialized {
            ProfileFiles::Complete => {
                let passphrase = self.existing_passphrase(member, &home, &keystore)?;
                (Self::load_profile(home)?, passphrase)
            }
            ProfileFiles::Empty => {
                let passphrase = self.new_passphrase(member, &home, protection)?;
                let alias = Alias::from_str(member.id())?;
                let profile =
                    RadicleProfile::init(home, alias, passphrase.clone(), Seed::generate())?;
                (profile, passphrase)
            }
            ProfileFiles::Partial => {
                return Err(Error::PartialProfile(home.path().to_path_buf()));
            }
        };

        Identity::from_profile(profile, passphrase)
    }

    fn load_existing(
        &self,
        member: Member,
        path: &Path,
        expected_did: Option<Did>,
    ) -> Result<Identity, Error> {
        let home = RadicleHome::load(path)?;
        let keystore = Keystore::new(&home.keys());
        let passphrase = self.existing_passphrase(member, &home, &keystore)?;
        let profile = Self::load_profile(home)?;
        let identity = Identity::from_profile(profile, passphrase)?;

        if let Some(expected) = expected_did
            && identity.did != expected
        {
            return Err(Error::DidMismatch {
                expected,
                actual: identity.did,
            });
        }

        Ok(identity)
    }

    fn load_profile(home: RadicleHome) -> Result<RadicleProfile, radicle::profile::Error> {
        let keystore = Keystore::new(&home.keys());
        let public_key = keystore
            .public_key()?
            .ok_or_else(|| radicle::profile::Error::NotFound(home.path().to_path_buf()))?;
        let config = Config::load(home.config().as_path())?;
        let storage = Storage::open(
            home.storage(),
            git::UserInfo {
                alias: config.alias().clone(),
                key: public_key,
            },
        )?;
        transport::local::register(storage.clone());

        Ok(RadicleProfile {
            home,
            storage,
            keystore,
            public_key,
            config,
        })
    }

    fn existing_passphrase(
        &self,
        member: Member,
        home: &RadicleHome,
        keystore: &Keystore,
    ) -> Result<Option<Passphrase>, Error> {
        if !keystore.is_encrypted()? {
            return Ok(None);
        }
        if let Some(passphrase) = radicle::profile::env::passphrase() {
            return Ok(Some(passphrase));
        }

        self.credentials
            .load(&CredentialId::new(member, home.path()))?
            .ok_or(Error::MissingPassphrase)
            .map(Some)
    }

    fn new_passphrase(
        &self,
        member: Member,
        home: &RadicleHome,
        protection: KeyProtection,
    ) -> Result<Option<Passphrase>, Error> {
        if protection == KeyProtection::Unencrypted {
            return Ok(None);
        }
        if let Some(passphrase) = radicle::profile::env::passphrase() {
            return Ok(Some(passphrase));
        }

        let id = CredentialId::new(member, home.path());
        if let Some(passphrase) = self.credentials.load(&id)? {
            return Ok(Some(passphrase));
        }
        let passphrase =
            Passphrase::from(Alphanumeric.sample_string(&mut rand::rng(), PASSPHRASE_LENGTH));
        self.credentials.store(&id, &passphrase)?;

        Ok(Some(passphrase))
    }

    fn profile_files(home: &RadicleHome, keystore: &Keystore) -> ProfileFiles {
        let files = [
            home.config().exists(),
            keystore.secret_key_path().exists(),
            keystore.public_key_path().is_some_and(|path| path.exists()),
        ];
        if files.into_iter().all(|exists| exists) {
            ProfileFiles::Complete
        } else if files.into_iter().all(|exists| !exists) {
            ProfileFiles::Empty
        } else {
            ProfileFiles::Partial
        }
    }
}

/// Source of a member's Radicle identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Provision or load the identity below the member home.
    Managed {
        /// Protection used only when provisioning a new key.
        protection: KeyProtection,
    },
    /// Load an identity from a dedicated existing Radicle home.
    RadicleHome {
        /// Existing Radicle home.
        home: PathBuf,
        /// Optional DID that the loaded key must derive.
        expected_did: Option<Did>,
    },
}

impl Default for Provider {
    fn default() -> Self {
        Self::Managed {
            protection: KeyProtection::default(),
        }
    }
}

/// Protection used when a new managed key is created.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum KeyProtection {
    /// Encrypt the key and store its generated passphrase in the system credential store.
    #[default]
    SystemCredential,
    /// Rely only on private filesystem permissions.
    Unencrypted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileFiles {
    Complete,
    Empty,
    Partial,
}

/// Error provisioning or loading a Radicle identity.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A Radicle home could not be created or loaded.
    #[error("Radicle home error: {0}")]
    Io(#[from] io::Error),
    /// The member or Radicle home could not be accessed safely.
    #[error("identity filesystem error: {0}")]
    Permissions(#[from] crate::identity::permissions::Error),
    /// Loading or initializing the Radicle profile failed.
    #[error("Radicle profile error: {0}")]
    Profile(#[from] radicle::profile::Error),
    /// The member name was not a valid Radicle alias.
    #[error("invalid Radicle alias for member: {0}")]
    Alias(#[from] radicle::node::AliasError),
    /// The operating-system credential could not be accessed.
    #[error("passphrase credential error: {0}")]
    Credential(#[from] crate::identity::credentials::Error),
    /// An encrypted key has no available passphrase.
    #[error(
        "encrypted identity key has no passphrase in RAD_PASSPHRASE or the system credential store"
    )]
    MissingPassphrase,
    /// A managed Radicle profile was only partly created.
    #[error("managed Radicle home '{0}' is only partially initialized")]
    PartialProfile(PathBuf),
    /// Loading and checking the private signing key failed.
    #[error("failed to load Radicle signing key: {0}")]
    SigningKey(#[from] MemorySignerError),
    /// Reading the OpenSSH key failed.
    #[error("Radicle keystore error: {0}")]
    Keystore(#[from] radicle::crypto::ssh::keystore::Error),
    /// Loading the signer from an SSH agent failed.
    #[error("failed to load Radicle signer: {0}")]
    Signer(#[from] radicle::profile::SignerError),
    /// The existing identity does not match its configured DID.
    #[error("loaded identity {actual} does not match expected identity {expected}")]
    DidMismatch {
        /// Configured DID.
        expected: Did,
        /// DID derived from the loaded key.
        actual: Did,
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    use radicle::{crypto::ssh::Passphrase, identity::RepoId};

    use crate::{
        home::MemberHome,
        identity::{
            CredentialId, CredentialStore, KeyProtection, Manager, Provider,
            credentials::Error as CredentialError, provider::Error,
        },
        member::Member,
    };

    const OTHER_DID: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[derive(Debug, Default)]
    struct MemoryCredentials {
        passphrase: RefCell<Option<String>>,
    }

    impl CredentialStore for MemoryCredentials {
        fn load(&self, _id: &CredentialId) -> Result<Option<Passphrase>, CredentialError> {
            Ok(self
                .passphrase
                .borrow()
                .as_ref()
                .map(|passphrase| Passphrase::from(passphrase.clone())))
        }

        fn store(
            &self,
            _id: &CredentialId,
            passphrase: &Passphrase,
        ) -> Result<(), CredentialError> {
            self.passphrase.replace(Some(passphrase.to_string()));
            Ok(())
        }
    }

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("radicle-team-identity-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn member_home(&self) -> MemberHome {
            MemberHome::at(
                &self.0,
                RepoId::from_urn(REPOSITORY).expect("repository should parse"),
                Member::Nico,
            )
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn provisions_and_reloads_managed_identity() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        let manager = Manager::new(MemoryCredentials::default());
        let provider = Provider::Managed {
            protection: KeyProtection::SystemCredential,
        };
        let identity = manager
            .load(Member::Nico, &home, &provider)
            .expect("identity should be provisioned");
        assert!(identity.memory_signer().is_some());
        let did = identity.did();
        drop(identity);

        let reloaded = manager
            .load(Member::Nico, &home, &provider)
            .expect("identity should reload");

        assert_eq!(reloaded.did(), did);
        assert!(reloaded.memory_signer().is_some());
    }

    #[test]
    fn provisions_an_unencrypted_identity() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        let identity = Manager::new(MemoryCredentials::default())
            .load(
                Member::Nico,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::Unencrypted,
                },
            )
            .expect("identity should be provisioned");

        assert!(
            !identity
                .profile()
                .keystore
                .is_encrypted()
                .expect("keystore should be readable")
        );
    }

    #[test]
    fn verifies_expected_did_for_existing_home() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        let manager = Manager::new(MemoryCredentials::default());
        manager
            .load(
                Member::Nico,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::Unencrypted,
                },
            )
            .expect("identity should be provisioned");

        assert!(matches!(
            manager.load(
                Member::Nico,
                &home,
                &Provider::RadicleHome {
                    home: home.radicle(),
                    expected_did: Some(OTHER_DID.parse().expect("DID should parse")),
                },
            ),
            Err(Error::DidMismatch { .. })
        ));
    }

    #[test]
    fn requires_a_passphrase_for_an_encrypted_existing_identity() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        let manager = Manager::new(MemoryCredentials::default());
        manager
            .load(Member::Nico, &home, &Provider::default())
            .expect("encrypted identity should be provisioned");

        assert!(matches!(
            Manager::new(MemoryCredentials::default()).load(
                Member::Nico,
                &home,
                &Provider::RadicleHome {
                    home: home.radicle(),
                    expected_did: None,
                },
            ),
            Err(Error::MissingPassphrase)
        ));
        let _identity = manager
            .load(
                Member::Nico,
                &home,
                &Provider::RadicleHome {
                    home: home.radicle(),
                    expected_did: None,
                },
            )
            .expect("stored passphrase should unlock the existing identity");
    }

    #[test]
    fn detects_partially_initialized_managed_home() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        fs::create_dir_all(home.radicle()).expect("Radicle directory should be created");
        fs::write(home.radicle().join("config.json"), "{}").expect("config should be written");

        assert!(matches!(
            Manager::new(MemoryCredentials::default()).load(
                Member::Nico,
                &home,
                &Provider::default()
            ),
            Err(Error::PartialProfile(_))
        ));
    }

    #[test]
    fn scopes_credentials_to_trusted_commands() {
        let directory = TestDirectory::new();
        let home = directory.member_home();
        let identity = Manager::new(MemoryCredentials::default())
            .load(
                Member::Nico,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::SystemCredential,
                },
            )
            .expect("identity should be provisioned");
        let mut trusted = Command::new("radicle-node");
        trusted.env(radicle::profile::env::RAD_SOCKET, "/unrelated/socket");
        identity.configure_radicle_command(&mut trusted);
        let mut untrusted = Command::new("model");
        untrusted.env(radicle::profile::env::RAD_PASSPHRASE, "secret");
        untrusted.env(radicle::profile::env::RAD_HOME, "/radicle");
        untrusted.env("SSH_AUTH_SOCK", "/agent");
        crate::identity::Identity::sanitize_command(&mut untrusted);

        assert!(trusted.get_envs().any(|(name, _)| name == "RAD_HOME"));
        assert!(
            trusted
                .get_envs()
                .any(|(name, value)| name == "RAD_SOCKET" && value.is_none())
        );
        assert!(
            untrusted
                .get_envs()
                .any(|(name, value)| name == "RAD_PASSPHRASE" && value.is_none())
        );
        assert!(
            untrusted
                .get_envs()
                .any(|(name, value)| name == "RAD_HOME" && value.is_none())
        );
        assert!(
            untrusted
                .get_envs()
                .any(|(name, value)| name == "SSH_AUTH_SOCK" && value.is_none())
        );
    }
}
