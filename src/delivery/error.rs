//! Delivery persistence and publication failures.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] crate::database::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Repository(#[from] radicle::storage::RepositoryError),
    #[error(transparent)]
    Cob(#[from] radicle::cob::store::Error),
    #[error(transparent)]
    Publication(#[from] crate::agent::PublishError),
    #[error(transparent)]
    Manifest(#[from] crate::manifest::Error),
    #[error("delivery worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
}
