//! Passphrase storage for managed Radicle identities.
//!
//! [`CredentialStore`] is the small boundary used by the identity provider to load and save
//! key-encryption passphrases. [`SystemCredentialStore`] maps it to the operating system's
//! credential service through `keyring`, with each new entry scoped by the member and signing
//! DID. Existing entries scoped by member and repository remain readable.
//!
//! This module never stores or reads the Radicle private key itself. It only supplies the
//! passphrase needed by the provider to create or unlock that key. Service-provided
//! `RAD_PASSPHRASE` values are handled directly by the provider and are not copied into the system
//! credential store.

use std::fmt;

use radicle::{
    crypto::ssh::Passphrase,
    identity::{Did, RepoId},
};
use keyring::{Entry, Error as KeyringError};

use crate::member::Member;

const SERVICE: &str = "radicle-team";

/// Storage used for encrypted managed-key passphrases.
pub trait CredentialStore: fmt::Debug {
    /// Loads a passphrase, returning `None` when no credential exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential store cannot be accessed.
    fn load(&self, id: &CredentialId) -> Result<Option<Passphrase>, Error>;

    /// Stores a passphrase.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential store cannot be accessed.
    fn store(&self, id: &CredentialId, passphrase: &Passphrase) -> Result<(), Error>;
}

/// Stable identifier of a member's stored key passphrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialId {
    account: String,
}

impl CredentialId {
    /// Names a legacy credential after the member and repository it belongs to.
    pub(super) fn new(member: Member, repository: RepoId) -> Self {
        Self {
            account: format!("{}:{repository}", member.id()),
        }
    }

    /// Names a credential after the signing identity, independent of its path or repository.
    pub(super) fn for_identity(member: Member, did: Did) -> Self {
        Self {
            account: format!("{}:{did}", member.id()),
        }
    }

    /// Returns the operating-system credential account name.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }
}

/// The operating system's native credential store.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCredentialStore;

impl SystemCredentialStore {
    fn entry(id: &CredentialId) -> Result<Entry, Error> {
        Entry::new(SERVICE, id.account()).map_err(|source| Error::Entry {
            account: id.account.clone(),
            source,
        })
    }
}

impl CredentialStore for SystemCredentialStore {
    fn load(&self, id: &CredentialId) -> Result<Option<Passphrase>, Error> {
        match Self::entry(id)?.get_password() {
            Ok(passphrase) => Ok(Some(Passphrase::from(passphrase))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(source) => Err(Error::Read {
                account: id.account.clone(),
                source,
            }),
        }
    }

    fn store(&self, id: &CredentialId, passphrase: &Passphrase) -> Result<(), Error> {
        Self::entry(id)?
            .set_password(passphrase)
            .map_err(|source| Error::Write {
                account: id.account.clone(),
                source,
            })
    }
}

/// Error accessing a passphrase credential.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Creating the platform credential entry failed.
    #[error("failed to open the operating-system credential for '{account}': {source}")]
    Entry {
        /// Credential account.
        account: String,
        /// Platform credential error.
        source: KeyringError,
    },
    /// Reading the platform credential failed.
    #[error("failed to read the operating-system credential for '{account}': {source}")]
    Read {
        /// Credential account.
        account: String,
        /// Platform credential error.
        source: KeyringError,
    },
    /// Writing the platform credential failed.
    #[error("failed to write the operating-system credential for '{account}': {source}")]
    Write {
        /// Credential account.
        account: String,
        /// Platform credential error.
        source: KeyringError,
    },
}

#[cfg(test)]
mod tests {
    use radicle::identity::RepoId;

    use crate::{identity::CredentialId, member::Member};

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const OTHER_REPOSITORY: &str = "rad:z42hL2jL4Tn8K5qgKbQ3xYrxLX9rU";

    #[test]
    fn credential_id_is_scoped_to_member_and_repository() {
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let other = RepoId::from_urn(OTHER_REPOSITORY).expect("repository should parse");
        let id = CredentialId::new(Member::Nico, repository);

        assert!(id.account().contains("nico"));
        assert_ne!(id, CredentialId::new(Member::Maya, repository));
        assert_ne!(id, CredentialId::new(Member::Nico, other));
        assert_eq!(
            id,
            CredentialId::new(Member::Nico, repository),
            "a member's passphrase must survive moving its directory"
        );
    }
}
