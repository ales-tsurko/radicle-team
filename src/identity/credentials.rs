//! Passphrase storage for managed Radicle identities.
//!
//! [`CredentialStore`] is the small boundary used by the identity provider to load and save
//! key-encryption passphrases. [`SystemCredentialStore`] maps it to the operating system's
//! credential service through `keyring`, with each entry scoped by the fixed member and its Radicle
//! home.
//!
//! This module never stores or reads the Radicle private key itself. It only supplies the
//! passphrase needed by the provider to create or unlock that key; service-provided
//! `RAD_PASSPHRASE` values are handled directly by the provider and are not copied into the system
//! credential store.

use std::{fmt, path::Path};

use keyring::{Entry, Error as KeyringError};
use radicle::crypto::ssh::Passphrase;

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
    pub(super) fn new(member: Member, radicle_home: &Path) -> Self {
        Self {
            account: format!("{}:{}", member.id(), radicle_home.display()),
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
    use crate::{home::MemberHome, identity::CredentialId, member::Member};

    #[test]
    fn credential_id_is_scoped_to_member_and_home() {
        let home = MemberHome::at("/team", Member::Nico);
        let id = CredentialId::new(Member::Nico, &home.radicle());
        let other_member = CredentialId::new(Member::Maya, &home.radicle());
        let other_home = MemberHome::at("/other-team", Member::Nico);
        let other_home = CredentialId::new(Member::Nico, &other_home.radicle());

        assert!(id.account().contains("nico"));
        assert_ne!(id, other_member);
        assert_ne!(id, other_home);
    }
}
