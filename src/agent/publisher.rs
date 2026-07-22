//! Publishing validated agent outcomes to Radicle collaborative objects.

use std::fmt;

use radicle::{
    cob::{
        EntryId, Op,
        issue::{self, IssueStream},
        patch::{self, PatchStream},
        store::access::ReadOnly,
        stream::CobStream as _,
    },
    crypto::{PublicKey, ssh::keystore::MemorySigner},
    identity::RepoId,
    node::{Handle as _, Node, device::Device},
    profile::Profile,
    storage::{ReadRepository as _, ReadStorage as _, SignRepository as _, refs::SignedRefs},
};

use crate::{
    agent::{DeliveryId, Reply, ReplyTarget},
    watcher::Subject,
};

/// Publishes agent replies with the managed member identity.
#[derive(Clone)]
pub(crate) struct Publisher<A, V> {
    profile: Profile,
    repository: RepoId,
    signer: Option<MemorySigner>,
    announcer: A,
    authorizer: V,
}

impl<A: fmt::Debug, V: fmt::Debug> fmt::Debug for Publisher<A, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Publisher")
            .field("repository", &self.repository)
            .field("signer_loaded", &self.signer.is_some())
            .field("announcer", &self.announcer)
            .field("authorizer", &self.authorizer)
            .finish_non_exhaustive()
    }
}

impl<A, V> Publisher<A, V> {
    pub(crate) const fn new(
        profile: Profile,
        repository: RepoId,
        signer: Option<MemorySigner>,
        announcer: A,
        authorizer: V,
    ) -> Self {
        Self {
            profile,
            repository,
            signer,
            announcer,
            authorizer,
        }
    }
}

impl<A: Announcer, V: Authorizer> Publisher<A, V> {
    pub(crate) fn author(&self) -> PublicKey {
        *self.profile.id()
    }

    pub(crate) async fn validate(&self, subject: Subject, reply: &Reply) -> Result<(), Error> {
        let publisher = self.clone();
        let target = reply.target;
        tokio::task::spawn_blocking(move || publisher.validate_blocking(subject, target))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    pub(crate) async fn publish(
        &self,
        subject: Subject,
        delivery: DeliveryId,
        reply: Reply,
    ) -> Result<EntryId, Error> {
        self.authorizer
            .authorize()
            .await
            .map_err(Error::Authorization)?;
        let publisher = self.clone();
        let operation = tokio::task::spawn_blocking(move || {
            publisher.publish_blocking(subject, delivery, reply)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        self.announce(self.author()).await?;

        Ok(operation)
    }

    pub(crate) async fn recover(
        &self,
        subject: Subject,
        delivery: DeliveryId,
        author: PublicKey,
        reply: &Reply,
    ) -> Result<bool, Error> {
        self.authorizer
            .authorize()
            .await
            .map_err(Error::Authorization)?;
        let publisher = self.clone();
        let reply = reply.clone();
        let operation = tokio::task::spawn_blocking(move || {
            publisher.recover_blocking(subject, delivery, author, reply)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        if operation.is_none() {
            return Ok(false);
        }
        self.announce(author).await?;
        Ok(true)
    }

    async fn announce(&self, namespace: PublicKey) -> Result<(), Error> {
        let mut announcer = self.announcer.clone();
        let repository = self.repository;
        tokio::task::spawn_blocking(move || announcer.announce(repository, namespace))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    fn marker(delivery: DeliveryId) -> String {
        format!("radicle-team reply {delivery}")
    }

    fn validate_blocking(&self, subject: Subject, target: ReplyTarget) -> Result<(), Error> {
        let repository = self.profile.storage.repository(self.repository)?;
        self.validate_target(&repository, subject, target)
    }

    fn validate_target(
        &self,
        repository: &radicle::storage::git::Repository,
        subject: Subject,
        target: ReplyTarget,
    ) -> Result<(), Error> {
        match (subject, target) {
            (Subject::Issue(issue_id), ReplyTarget::Issue { reply_to }) => {
                let issues = issue::Issues::open(repository, ReadOnly)?;
                let issue = issues
                    .get(&issue_id)?
                    .ok_or(Error::IssueParentUnavailable {
                        issue: issue_id,
                        parent: reply_to.unwrap_or(*issue_id),
                    })?;
                let parent = reply_to.unwrap_or(*issue_id);
                if issue.thread().comment(&parent).is_none() {
                    return Err(Error::IssueParentUnavailable {
                        issue: issue_id,
                        parent,
                    });
                }
                Ok(())
            }
            (Subject::Patch(patch_id), ReplyTarget::Revision { revision, reply_to }) => self
                .validate_patch_target(
                    repository,
                    patch_id,
                    PatchTarget::Revision { revision, reply_to },
                ),
            (Subject::Patch(patch_id), ReplyTarget::Review { review, reply_to }) => self
                .validate_patch_target(
                    repository,
                    patch_id,
                    PatchTarget::Review { review, reply_to },
                ),
            (subject, target) => Err(Error::InvalidTarget { subject, target }),
        }
    }

    fn validate_patch_target(
        &self,
        repository: &radicle::storage::git::Repository,
        patch_id: radicle::cob::ObjectId,
        target: PatchTarget,
    ) -> Result<(), Error> {
        let patches = patch::Patches::open(repository, ReadOnly)?;
        let valid = patches
            .get(&patch_id)?
            .is_some_and(|patch| target.exists(&patch));
        if valid {
            Ok(())
        } else {
            Err(Error::PatchTargetUnavailable {
                patch: patch_id,
                target,
            })
        }
    }

    fn publish_blocking(
        &self,
        subject: Subject,
        delivery: DeliveryId,
        reply: Reply,
    ) -> Result<EntryId, Error> {
        let repository = self.profile.storage.repository(self.repository)?;
        let marker = Self::marker(delivery);
        let signer = match self.signer.clone() {
            Some(signer) => Device::from(signer).boxed(),
            None => self.profile.signer()?,
        };
        if let Some(operation) =
            self.published(&repository, subject, &marker, self.author(), &reply)?
        {
            let _signed = repository.sign_refs(&signer)?;
            return Ok(operation);
        }
        self.validate_target(&repository, subject, reply.target)?;

        match (subject, reply.target) {
            (Subject::Issue(issue), ReplyTarget::Issue { reply_to }) => {
                self.publish_issue(&repository, &signer, issue, reply.body, reply_to, &marker)
            }
            (Subject::Patch(patch), ReplyTarget::Revision { revision, reply_to }) => self
                .publish_patch(
                    &repository,
                    &signer,
                    patch,
                    reply.body,
                    PatchTarget::Revision { revision, reply_to },
                    &marker,
                ),
            (Subject::Patch(patch), ReplyTarget::Review { review, reply_to }) => self
                .publish_patch(
                    &repository,
                    &signer,
                    patch,
                    reply.body,
                    PatchTarget::Review { review, reply_to },
                    &marker,
                ),
            (subject, target) => Err(Error::InvalidTarget { subject, target }),
        }
    }

    fn recover_blocking(
        &self,
        subject: Subject,
        delivery: DeliveryId,
        author: PublicKey,
        reply: Reply,
    ) -> Result<Option<EntryId>, Error> {
        let repository = self.profile.storage.repository(self.repository)?;
        let operation = self.published(
            &repository,
            subject,
            &Self::marker(delivery),
            author,
            &reply,
        )?;
        let Some(operation) = operation else {
            return Ok(None);
        };
        if author == self.author() {
            let signer = match self.signer.clone() {
                Some(signer) => Device::from(signer).boxed(),
                None => self.profile.signer()?,
            };
            let _signed = repository.sign_refs(&signer)?;
        } else if !Self::is_signed(&repository, author, operation)? {
            return Ok(None);
        }
        Ok(Some(operation))
    }

    fn is_signed(
        repository: &radicle::storage::git::Repository,
        author: PublicKey,
        operation: EntryId,
    ) -> Result<bool, Error> {
        let Some(signed) = SignedRefs::load(author, repository)? else {
            return Ok(false);
        };
        let operation_oid = operation;
        for (_name, tip) in signed.iter() {
            if *tip == operation_oid || repository.is_ancestor_of(operation_oid, *tip)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn publish_issue(
        &self,
        repository: &radicle::storage::git::Repository,
        signer: &radicle::node::device::BoxedDevice,
        issue_id: radicle::cob::ObjectId,
        body: String,
        reply_to: Option<EntryId>,
        marker: &str,
    ) -> Result<EntryId, Error> {
        let mut issues = issue::Cache::no_cache(repository, signer)?;
        let mut issue = issues.get_mut(&issue_id)?;
        let parent = reply_to.unwrap_or(*issue_id);
        if issue.thread().comment(&parent).is_none() {
            return Err(Error::IssueParentUnavailable {
                issue: issue_id,
                parent,
            });
        }

        Ok(issue.transaction(marker, |transaction| {
            transaction.comment(body, parent, Vec::new())
        })?)
    }

    fn publish_patch(
        &self,
        repository: &radicle::storage::git::Repository,
        signer: &radicle::node::device::BoxedDevice,
        patch_id: radicle::cob::ObjectId,
        body: String,
        target: PatchTarget,
        marker: &str,
    ) -> Result<EntryId, Error> {
        let mut patches = patch::Cache::no_cache(repository, signer)?;
        let mut patch = patches.get_mut(&patch_id)?;
        if !target.exists(&patch) {
            return Err(Error::PatchTargetUnavailable {
                patch: patch_id,
                target,
            });
        }

        match target {
            PatchTarget::Revision { revision, reply_to } => patch
                .transaction(marker, |transaction| {
                    transaction.comment(revision, body, reply_to, None, Vec::new())
                })
                .map_err(Error::Patch),
            PatchTarget::Review { review, reply_to } => patch
                .transaction(marker, |transaction| {
                    transaction.review_comment(review, body, None, reply_to, Vec::new())
                })
                .map_err(Error::Patch),
        }
    }

    fn published(
        &self,
        repository: &radicle::storage::git::Repository,
        subject: Subject,
        marker: &str,
        author: PublicKey,
        reply: &Reply,
    ) -> Result<Option<EntryId>, Error> {
        match subject {
            Subject::Issue(issue) => {
                let Some(expected) = Self::issue_reply_action(issue, reply) else {
                    return Ok(None);
                };
                self.find_marker(
                    repository,
                    IssueStream::init(issue, repository).all()?,
                    marker,
                    author,
                    |operation| Self::matches_action(operation, &expected),
                )
            }
            Subject::Patch(patch) => {
                let Some(expected) = Self::patch_reply_action(reply) else {
                    return Ok(None);
                };
                self.find_marker(
                    repository,
                    PatchStream::init(patch, repository).all()?,
                    marker,
                    author,
                    |operation| Self::matches_action(operation, &expected),
                )
            }
        }
    }

    fn issue_reply_action(issue: issue::IssueId, reply: &Reply) -> Option<issue::Action> {
        let ReplyTarget::Issue { reply_to } = reply.target else {
            return None;
        };
        Some(issue::Action::Comment {
            body: reply.body.clone(),
            reply_to: Some(reply_to.unwrap_or(*issue)),
            embeds: Vec::new(),
        })
    }

    fn patch_reply_action(reply: &Reply) -> Option<patch::Action> {
        match reply.target {
            ReplyTarget::Revision { revision, reply_to } => Some(patch::Action::RevisionComment {
                revision,
                location: None,
                body: reply.body.clone(),
                reply_to,
                embeds: Vec::new(),
            }),
            ReplyTarget::Review { review, reply_to } => Some(patch::Action::ReviewComment {
                review,
                body: reply.body.clone(),
                location: None,
                reply_to,
                embeds: Vec::new(),
            }),
            ReplyTarget::Issue { .. } => None,
        }
    }

    fn matches_action<T: PartialEq>(operation: &Op<T>, expected: &T) -> bool {
        operation.actions.len() == 1 && operation.actions.head == *expected
    }

    fn find_marker<T>(
        &self,
        repository: &radicle::storage::git::Repository,
        operations: impl Iterator<Item = Result<Op<T>, radicle::cob::stream::error::Ops>>,
        marker: &str,
        author: PublicKey,
        matches_reply: impl Fn(&Op<T>) -> bool,
    ) -> Result<Option<EntryId>, Error> {
        for operation in operations {
            let operation = operation?;
            if operation.author != author {
                continue;
            }
            if !matches_reply(&operation) {
                continue;
            }
            let commit = repository.backend.find_commit(operation.id.into())?;
            if commit
                .message()
                .is_some_and(|message| message.lines().next() == Some(marker))
            {
                return Ok(Some(operation.id));
            }
        }
        Ok(None)
    }
}

pub(crate) trait Announcer: Clone + std::fmt::Debug + Send + Sync + 'static {
    fn announce(&mut self, repository: RepoId, namespace: PublicKey) -> Result<(), Error>;
}

#[async_trait::async_trait]
pub(crate) trait Authorizer: Clone + std::fmt::Debug + Send + Sync + 'static {
    async fn authorize(&self) -> Result<(), String>;
}

impl Announcer for Node {
    fn announce(&mut self, repository: RepoId, namespace: PublicKey) -> Result<(), Error> {
        let _announced = self.announce_refs_for(repository, [namespace])?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PatchTarget {
    Revision {
        revision: patch::RevisionId,
        reply_to: Option<EntryId>,
    },
    Review {
        review: patch::ReviewId,
        reply_to: Option<EntryId>,
    },
}

impl PatchTarget {
    fn exists(&self, patch: &patch::Patch) -> bool {
        match self {
            Self::Revision { revision, reply_to } => {
                patch.revision(revision).is_some_and(|value| {
                    reply_to.is_none_or(|parent| value.discussion().comment(&parent).is_some())
                })
            }
            Self::Review { review, reply_to } => patch
                .revisions()
                .flat_map(|(_id, revision)| revision.reviews())
                .find_map(|(_author, candidate)| (candidate.id() == *review).then_some(candidate))
                .is_some_and(|value| {
                    reply_to.is_none_or(|parent| {
                        value.comments().any(|(comment, _value)| *comment == parent)
                    })
                }),
        }
    }
}

/// Error produced while publishing an agent outcome.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("reply target {target:?} is invalid for {subject:?}")]
    InvalidTarget {
        subject: Subject,
        target: ReplyTarget,
    },
    #[error("reply target {target:?} is unavailable in patch {patch}")]
    PatchTargetUnavailable {
        patch: radicle::cob::ObjectId,
        target: PatchTarget,
    },
    #[error("reply parent {parent} is unavailable in issue {issue}")]
    IssueParentUnavailable {
        issue: radicle::cob::ObjectId,
        parent: EntryId,
    },
    #[error("failed to announce published reply: {0}")]
    Node(#[from] radicle::node::Error),
    #[error("reply publication is no longer authorized: {0}")]
    Authorization(String),
    #[error("failed to open Radicle repository: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    #[error("failed to load member signer: {0}")]
    Signer(#[from] radicle::profile::SignerError),
    #[error("failed to open collaborative object cache: {0}")]
    Profile(#[from] radicle::profile::Error),
    #[error("failed to load issue: {0}")]
    IssueCache(#[from] issue::cache::Error),
    #[error("failed to load patch: {0}")]
    PatchCache(#[from] patch::cache::Error),
    #[error("failed to read collaborative object: {0}")]
    CobStore(#[from] radicle::cob::store::Error),
    #[error("failed to open collaborative object operation stream: {0}")]
    Stream(#[from] radicle::cob::stream::error::Stream),
    #[error("failed to read collaborative object operation: {0}")]
    Operation(#[from] radicle::cob::stream::error::Ops),
    #[error("failed to read collaborative object commit: {0}")]
    Git(#[from] radicle::git::raw::Error),
    #[error("failed to read signed references: {0}")]
    SignedRefs(#[from] radicle::storage::refs::sigrefs::read::error::Read),
    #[error("failed to publish issue reply: {0}")]
    Issue(#[from] issue::Error),
    #[error("failed to publish patch reply: {0}")]
    Patch(#[from] patch::Error),
    #[error("reply publisher task failed: {0}")]
    Task(String),
}

impl Error {
    pub(crate) const fn is_invalid_target(&self) -> bool {
        matches!(
            self,
            Self::InvalidTarget { .. }
                | Self::PatchTargetUnavailable { .. }
                | Self::IssueParentUnavailable { .. }
        )
    }
}
