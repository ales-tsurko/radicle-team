//! Errors inspecting or recording member status.

use std::path::PathBuf;

/// Failure reading member status or recording runtime activity.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The member directory could not be located or read.
    #[error(transparent)]
    Home(#[from] crate::home::Error),
    /// The recorded member is invalid.
    #[error(transparent)]
    Member(#[from] crate::member::UnknownMember),
    /// The recorded repository is invalid.
    #[error(transparent)]
    Repository(#[from] radicle::identity::IdError),
    /// The member's public identity could not be read.
    #[error("could not read member public identity: {0}")]
    PublicIdentity(#[from] radicle::crypto::ssh::keystore::Error),
    /// An identity-only directory has no repository yet.
    #[error(
        "member home '{0}' has no repository binding; complete setup with --repo and --home first"
    )]
    UnboundIdentity(PathBuf),
    /// The process ownership check failed.
    #[error(transparent)]
    Lock(#[from] crate::identity::LockError),
    /// Reading or writing SQLite state failed.
    #[error("member status database error: {0}")]
    Database(#[from] crate::database::Error),
    /// The stored snapshot could not be decoded.
    #[error("invalid member status snapshot: {0}")]
    Snapshot(#[from] serde_json::Error),
    /// The database path could not be inspected.
    #[error("could not inspect member state: {0}")]
    Io(#[from] std::io::Error),
    /// The requested task is no longer retained.
    #[error(
        "unfinished task {0} not found\nOnly unfinished tasks are retained. Use `rad-team task \
         ls` to see them."
    )]
    MissingTask(i64),
}
