//! Resolving a prepared member without unlocking its signing key.

use std::path::PathBuf;

use radicle::{
    identity::{Did, RepoId},
    profile::Home,
    storage::ReadStorage as _,
};

use crate::{
    agent::Role,
    cli::provider::Error,
    home::MemberHome,
    manifest::{self, Manifest, ValidatedBinding},
    member::Member,
};

#[derive(Debug)]
pub(crate) struct Context {
    pub(crate) home: MemberHome,
    pub(crate) member: Member,
    pub(crate) repository: RepoId,
    pub(crate) did: Did,
    pub(crate) binding: ValidatedBinding,
    pub(crate) canonical: bool,
}

impl Context {
    pub(crate) fn load(path: Option<PathBuf>) -> Result<Self, Error> {
        let home = match path {
            Some(path) => MemberHome::at(path),
            None => MemberHome::discover()?,
        };
        let marker = home.marker()?;
        let home = MemberHome::at(home.path().canonicalize()?);
        let member = marker.member()?;
        let repository = marker
            .repository()?
            .ok_or_else(|| Error::Unbound(home.path().into()))?;
        // Loading public profile data neither generates a key nor asks for its passphrase.
        let profile =
            crate::identity::Manager::<crate::identity::SystemCredentialStore>::load_profile(
                Home::load(home.identity())?,
            )?;
        let did = profile.did();
        let stored = profile.storage.repository(repository)?;
        let canonical = Manifest::load_optional_repository(&stored)?
            .map(|manifest| manifest.validate_member(member, did))
            .transpose();
        let binding = match canonical {
            Ok(Some(binding)) => Some(binding),
            Ok(None) | Err(manifest::Error::Missing { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let canonical = binding.is_some();
        let binding = match binding {
            Some(binding) => binding,
            None => {
                let path = home.worktree(Role::Coder).join(manifest::FILE_NAME);
                if !path.exists() {
                    return Err(Error::NotPrepared);
                }
                Manifest::load_file(&path)?.validate_member(member, did)?
            }
        };
        Ok(Self {
            home,
            member,
            repository,
            did,
            binding,
            canonical,
        })
    }

    pub(crate) fn provider(&self) -> Result<&str, Error> {
        self.binding.provider().ok_or(Error::NoProvider)
    }
}
