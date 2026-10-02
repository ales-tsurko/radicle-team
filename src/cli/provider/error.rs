//! Provider command errors with their original causes.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Provider definitions could not be read.
    #[error(transparent)]
    Provider(#[from] crate::provider::Error),
    /// Saved authorization could not be read or written.
    #[error(transparent)]
    Credentials(#[from] crate::provider::CredentialsError),
    /// The member home could not be resolved.
    #[error(transparent)]
    Home(#[from] crate::home::Error),
    /// The marker contains an unknown member.
    #[error(transparent)]
    Member(#[from] crate::member::UnknownMember),
    /// The marker contains an invalid repository ID.
    #[error(transparent)]
    RepositoryId(#[from] radicle::identity::IdError),
    /// Reading the member's public identity failed.
    #[error(transparent)]
    Profile(#[from] radicle::profile::Error),
    /// The stored repository could not be read.
    #[error(transparent)]
    Storage(#[from] radicle::storage::RepositoryError),
    /// The manifest does not authorize this member.
    #[error(transparent)]
    Manifest(#[from] crate::manifest::Error),
    /// A filesystem operation or interactive prompt failed.
    #[error("provider command failed: {0}")]
    Io(#[from] std::io::Error),
    /// Native subscription sign-in failed.
    #[error(transparent)]
    Authentication(#[from] crate::agent::authentication::Error),
    /// Publication through the member checkout failed.
    #[error(transparent)]
    Checkout(Box<crate::cli::setup::Error>),
    /// Announcing the resulting patch failed.
    #[error(transparent)]
    Application(Box<crate::Error>),
    /// The member has only an identity.
    #[error("member home '{0}' has no repository; complete setup with --repo first")]
    Unbound(PathBuf),
    /// The canonical manifest has no provider selection.
    #[error("no provider configured for this member; run `rad-team provider select`")]
    NoProvider,
    /// Local initialization has not produced a manifest proposal yet.
    #[error("complete setup before managing this member's provider")]
    NotPrepared,
    /// Selection requires an approved member binding.
    #[error("merge this member's setup patch before selecting another provider")]
    PendingSetup,
    /// A provider takes no authorization.
    #[error("provider '{0}' needs no authorization")]
    NothingToAuthorize(String),
}

impl From<crate::cli::setup::Error> for Error {
    fn from(error: crate::cli::setup::Error) -> Self {
        Self::Checkout(Box::new(error))
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Application(Box::new(error))
    }
}
