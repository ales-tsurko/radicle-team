//! Validation of journaled mutations during replay.

use radicle::cob::{
    Embed, EntryId, ObjectId, Op,
    common::Uri,
    issue::{self, Action as IssueAction, IssueStream, Issues},
    patch::{self, Action as PatchAction, PatchStream, Patches},
    store::access::ReadOnly,
    stream::CobStream as _,
};

use crate::mcp::protocol::{
    CommentOperation, CommentTargetKind, IssueUpdate, PatchCommitAction, PatchUpdate, SubjectKind,
    stringify,
};

#[derive(Clone, Copy)]
enum RecordedEdit<'a> {
    Issue {
        action: &'a IssueAction,
        issue: ObjectId,
    },
    Patch {
        action: &'a PatchAction,
        revision: Option<patch::RevisionId>,
    },
}

pub(super) fn issue_edit_operation(
    repository: &CobRepository,
    issue: ObjectId,
    operation: EntryId,
    title: Option<&radicle::cob::Title>,
    description: Option<&str>,
) -> Result<bool, String> {
    edit_operation(
        find_issue_operation(repository, issue, operation),
        title,
        description,
        |action| RecordedEdit::Issue { action, issue },
    )
}

fn find_issue_operation(
    repository: &CobRepository,
    issue: ObjectId,
    operation: EntryId,
) -> Result<Option<Op<IssueAction>>, String> {
    find_cob_operation(IssueStream::init(issue, repository).all(), operation)
}

fn find_patch_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
) -> Result<Option<Op<PatchAction>>, String> {
    find_cob_operation(PatchStream::init(patch, repository).all(), operation)
}

fn find_cob_operation<A>(
    operations: Result<
        impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
        radicle::cob::stream::error::Stream,
    >,
    operation: EntryId,
) -> Result<Option<Op<A>>, String> {
    for candidate in operations.map_err(stringify)? {
        let candidate = candidate.map_err(stringify)?;
        if candidate.id == operation {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

pub(super) fn issue_create_operation(
    repository: &CobRepository,
    operation: EntryId,
    title: &radicle::cob::Title,
    description: &str,
    labels: &[radicle::cob::Label],
    assignees: &[radicle::identity::Did],
) -> Result<bool, String> {
    Ok(
        find_issue_operation(repository, ObjectId::from(operation), operation)?.is_some_and(
            |operation| {
                operation.actions.iter().any(
                    |action| matches!(action, IssueAction::Edit { title: value } if value == title),
                ) && operation.actions.iter().any(|action| {
                    matches!(
                        action,
                        IssueAction::Comment {
                            body,
                            reply_to: None,
                            ..
                        } if body == description
                    )
                }) && (labels.is_empty()
                    || operation.actions.iter().any(|action| {
                        matches!(
                            action,
                            IssueAction::Label { labels: actual }
                                if actual.iter().eq(labels)
                        )
                    }))
                    && (assignees.is_empty()
                        || operation.actions.iter().any(|action| {
                            matches!(
                                action,
                                IssueAction::Assign { assignees: actual }
                                    if actual.iter().eq(assignees)
                            )
                        }))
            },
        ),
    )
}

pub(super) struct CommentRecovery<'a> {
    pub(super) kind: SubjectKind,
    pub(super) target: CommentTargetKind,
    pub(super) subject: ObjectId,
    pub(super) entity: Option<EntryId>,
    pub(super) comment: EntryId,
    pub(super) operation: CommentOperation,
    pub(super) body: Option<&'a str>,
    pub(super) embeds: &'a [Embed<Uri>],
}

#[derive(Clone, Copy)]
pub(super) struct ReactionRecovery<'a> {
    pub(super) kind: SubjectKind,
    pub(super) target: CommentTargetKind,
    pub(super) subject: ObjectId,
    pub(super) entity: Option<EntryId>,
    pub(super) comment: Option<EntryId>,
    pub(super) reaction: radicle::cob::Reaction,
    pub(super) location: Option<&'a radicle::cob::CodeLocation>,
    pub(super) active: bool,
}

#[derive(Clone, Copy)]
enum PatchReactionTarget {
    Revision(patch::RevisionId),
    Review(patch::ReviewId),
}

#[derive(Clone, Copy)]
enum ReactionOperationTarget {
    Issue(EntryId),
    Patch(PatchReactionTarget),
    Missing(&'static str),
    Invalid,
}

impl ReactionOperationTarget {
    fn revision(entity: Option<EntryId>) -> Self {
        entity
            .map(patch::RevisionId::from)
            .map(PatchReactionTarget::Revision)
            .map(Self::Patch)
            .unwrap_or(Self::Missing("entity revision is required"))
    }

    fn review(entity: Option<EntryId>) -> Self {
        entity
            .map(patch::ReviewId::from)
            .map(PatchReactionTarget::Review)
            .map(Self::Patch)
            .unwrap_or(Self::Missing("entity review is required"))
    }

    fn recorded(
        self,
        recovery: &ReactionRecovery<'_>,
        repository: &CobRepository,
        operation: EntryId,
    ) -> Result<bool, String> {
        match self {
            Self::Issue(comment) => operation_has_action(
                find_issue_operation(repository, recovery.subject, operation),
                |action| recovery.issue_action_matches(action, comment),
            ),
            Self::Patch(target) => operation_has_action(
                find_patch_operation(repository, recovery.subject, operation),
                |action| recovery.patch_action_matches(action, target),
            ),
            Self::Missing(message) => Err(message.to_owned()),
            Self::Invalid => Ok(false),
        }
    }
}

type CobRepository = radicle::storage::git::Repository;
type CommentStateResult = Result<Option<CommentState>, String>;

struct CommentState {
    body: String,
    embeds: Vec<Embed<Uri>>,
}

impl CommentState {
    fn from_comment<L>(comment: &radicle::cob::thread::Comment<L>) -> Self {
        Self {
            body: comment.body().to_owned(),
            embeds: comment.embeds().to_vec(),
        }
    }
}

impl<'a> CommentRecovery<'a> {
    pub(super) fn applied(&self, repository: &CobRepository) -> Result<bool, String> {
        let actual = match (self.kind, self.target) {
            (SubjectKind::Issue, CommentTargetKind::Issue) => self.issue_state(repository),
            (SubjectKind::Patch, CommentTargetKind::Revision) => self.revision_state(repository),
            (SubjectKind::Patch, CommentTargetKind::Review) => self.review_state(repository),
            _ => return Ok(false),
        };
        actual.map(|actual| self.state_matches(actual.as_ref()))
    }

    pub(super) fn operation_recorded(
        &self,
        repository: &CobRepository,
        operation: EntryId,
    ) -> Result<bool, String> {
        match (self.kind, self.target) {
            (SubjectKind::Issue, CommentTargetKind::Issue) => {
                let expected = self.recorded();
                Ok(
                    find_issue_operation(repository, self.subject, operation)?.is_some_and(
                        |operation| {
                            operation
                                .actions
                                .iter()
                                .filter_map(issue_comment)
                                .any(|actual| actual == expected)
                        },
                    ),
                )
            }
            (SubjectKind::Patch, CommentTargetKind::Revision) => {
                self.patch_operation(repository, operation, PatchCommentTarget::Revision)
            }
            (SubjectKind::Patch, CommentTargetKind::Review) => {
                self.patch_operation(repository, operation, PatchCommentTarget::Review)
            }
            _ => Ok(false),
        }
    }

    fn recorded(&self) -> RecordedComment<'_> {
        match self.operation {
            CommentOperation::Create => {
                RecordedComment::Create(self.body.expect("body was validated"), self.embeds)
            }
            CommentOperation::Edit => RecordedComment::Edit(
                self.comment,
                self.body.expect("body was validated"),
                self.embeds,
            ),
            CommentOperation::Redact => RecordedComment::Redact(self.comment),
        }
    }

    fn patch_operation(
        &self,
        repository: &CobRepository,
        operation: EntryId,
        target: PatchCommentTarget,
    ) -> Result<bool, String> {
        let entity = self
            .entity
            .ok_or_else(|| "comment entity is required".to_owned())?;
        let expected = RecordedPatchComment {
            entity: match target {
                PatchCommentTarget::Revision => {
                    PatchCommentEntity::Revision(patch::RevisionId::from(entity))
                }
                PatchCommentTarget::Review => {
                    PatchCommentEntity::Review(patch::ReviewId::from(entity))
                }
            },
            comment: self.recorded(),
        };
        Ok(
            find_patch_operation(repository, self.subject, operation)?.is_some_and(|operation| {
                operation
                    .actions
                    .iter()
                    .filter_map(|action| {
                        revision_comment(action).or_else(|| review_comment(action))
                    })
                    .any(|actual| actual == expected)
            }),
        )
    }

    fn state_matches(&self, actual: Option<&CommentState>) -> bool {
        match self.operation {
            CommentOperation::Create | CommentOperation::Edit => actual.is_some_and(|actual| {
                actual.body == self.body.expect("body was validated")
                    && actual.embeds == self.embeds
            }),
            CommentOperation::Redact => actual.is_none(),
        }
    }

    fn issue_state(&self, repository: &CobRepository) -> CommentStateResult {
        Ok(load_issue(repository, self.subject)?.and_then(|issue| {
            issue
                .thread()
                .comment(&self.comment)
                .map(CommentState::from_comment)
        }))
    }

    fn revision_state(&self, repository: &CobRepository) -> CommentStateResult {
        let revision = self
            .entity
            .ok_or_else(|| "entity revision is required".to_owned())?;
        Ok(load_patch(repository, self.subject)?.and_then(|patch| {
            patch
                .revision(&patch::RevisionId::from(revision))
                .and_then(|value| value.discussion().comment(&self.comment))
                .map(CommentState::from_comment)
        }))
    }

    fn review_state(&self, repository: &CobRepository) -> CommentStateResult {
        let review = self
            .entity
            .ok_or_else(|| "entity review is required".to_owned())?;
        Ok(load_patch(repository, self.subject)?.and_then(|patch| {
            find_review(&patch, patch::ReviewId::from(review)).and_then(|review| {
                review
                    .comments()
                    .find(|(candidate, _value)| **candidate == self.comment)
                    .map(|(_candidate, value)| CommentState::from_comment(value))
            })
        }))
    }
}

impl ReactionRecovery<'_> {
    pub(super) fn operation_recorded(
        &self,
        repository: &CobRepository,
        operation: EntryId,
    ) -> Result<bool, String> {
        self.operation_target()
            .recorded(self, repository, operation)
    }

    fn operation_target(&self) -> ReactionOperationTarget {
        match (self.kind, self.target) {
            (SubjectKind::Issue, CommentTargetKind::Issue) => {
                ReactionOperationTarget::Issue(self.comment.unwrap_or(*self.subject))
            }
            (SubjectKind::Patch, CommentTargetKind::Revision) => {
                ReactionOperationTarget::revision(self.entity)
            }
            (SubjectKind::Patch, CommentTargetKind::Review) => {
                ReactionOperationTarget::review(self.entity)
            }
            _ => ReactionOperationTarget::Invalid,
        }
    }

    fn issue_action_matches(&self, action: &IssueAction, comment: EntryId) -> bool {
        matches!(
            action,
            IssueAction::CommentReact {
                id,
                reaction,
                active,
            } if *id == comment
                && *reaction == self.reaction
                && *active == self.active
        )
    }

    fn patch_action_matches(&self, action: &PatchAction, target: PatchReactionTarget) -> bool {
        match (target, self.comment) {
            (PatchReactionTarget::Revision(revision), Some(comment)) => matches!(
                action,
                PatchAction::RevisionCommentReact {
                    revision: actual_revision,
                    comment: actual_comment,
                    reaction,
                    active,
                } if *actual_revision == revision
                    && *actual_comment == comment
                    && *reaction == self.reaction
                    && *active == self.active
            ),
            (PatchReactionTarget::Revision(revision), None) => matches!(
                action,
                PatchAction::RevisionReact {
                    revision: actual_revision,
                    location,
                    reaction,
                    active,
                } if *actual_revision == revision
                    && location.as_ref() == self.location
                    && *reaction == self.reaction
                    && *active == self.active
            ),
            (PatchReactionTarget::Review(review), Some(comment)) => matches!(
                    action,
                    PatchAction::ReviewCommentReact {
                        review: actual_review,
                        comment: actual_comment,
                        reaction,
                        active,
                    } if *actual_review == review
                        && *actual_comment == comment
                        && *reaction == self.reaction
                        && *active == self.active
            ),
            (PatchReactionTarget::Review(review), None) => matches!(
                action,
                PatchAction::ReviewReact {
                    review: actual_review,
                    reaction,
                    active,
                } if *actual_review == review
                    && *reaction == self.reaction
                    && *active == self.active
            ),
        }
    }

    pub(super) fn applied(
        &self,
        repository: &CobRepository,
        author: radicle::crypto::PublicKey,
    ) -> Result<bool, String> {
        match (self.kind, self.target) {
            (SubjectKind::Issue, CommentTargetKind::Issue) => {
                let comment = self.comment.unwrap_or(*self.subject);
                Ok(load_issue(repository, self.subject)?
                    .and_then(|issue| {
                        issue.thread().comment(&comment).map(|comment| {
                            comment
                                .reactions()
                                .get(&self.reaction)
                                .is_some_and(|authors| authors.contains(&&author))
                                == self.active
                        })
                    })
                    .unwrap_or(false))
            }
            (SubjectKind::Patch, CommentTargetKind::Revision) => {
                let revision = patch::RevisionId::from(
                    self.entity
                        .ok_or_else(|| "entity revision is required".to_owned())?,
                );
                Ok(load_patch(repository, self.subject)?
                    .and_then(|patch| {
                        patch
                            .revision(&revision)
                            .map(|revision| match self.comment {
                                Some(comment) => revision
                                    .discussion()
                                    .comment(&comment)
                                    .is_some_and(|comment| {
                                        comment
                                            .reactions()
                                            .get(&self.reaction)
                                            .is_some_and(|authors| authors.contains(&&author))
                                            == self.active
                                    }),
                                None => {
                                    revision
                                        .reactions()
                                        .iter()
                                        .find_map(|(location, reactions)| {
                                            (location.as_ref() == self.location)
                                                .then_some(reactions)
                                        })
                                        .is_some_and(|reactions| {
                                            reactions.contains(&(author, self.reaction))
                                        })
                                        == self.active
                                }
                            })
                    })
                    .unwrap_or(false))
            }
            (SubjectKind::Patch, CommentTargetKind::Review) => {
                let review = patch::ReviewId::from(
                    self.entity
                        .ok_or_else(|| "entity review is required".to_owned())?,
                );
                Ok(load_patch(repository, self.subject)?
                    .and_then(|patch| {
                        find_review(&patch, review).map(|review| match self.comment {
                            Some(comment) => review
                                .comments()
                                .find_map(|(candidate, value)| {
                                    (*candidate == comment).then_some(value)
                                })
                                .is_some_and(|comment| {
                                    comment
                                        .reactions()
                                        .get(&self.reaction)
                                        .is_some_and(|authors| authors.contains(&&author))
                                        == self.active
                                }),
                            None => {
                                review.reactions().contains(&(author, self.reaction)) == self.active
                            }
                        })
                    })
                    .unwrap_or(false))
            }
            _ => Ok(false),
        }
    }
}

#[derive(PartialEq, Eq)]
enum RecordedComment<'a> {
    Create(&'a str, &'a [Embed<Uri>]),
    Edit(EntryId, &'a str, &'a [Embed<Uri>]),
    Redact(EntryId),
}

fn issue_comment(action: &IssueAction) -> Option<RecordedComment<'_>> {
    match action {
        IssueAction::Comment { body, embeds, .. } => Some(RecordedComment::Create(body, embeds)),
        IssueAction::CommentEdit {
            id, body, embeds, ..
        } => Some(RecordedComment::Edit(*id, body, embeds)),
        IssueAction::CommentRedact { id } => Some(RecordedComment::Redact(*id)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum PatchCommentTarget {
    Revision,
    Review,
}

#[derive(PartialEq, Eq)]
enum PatchCommentEntity {
    Revision(patch::RevisionId),
    Review(patch::ReviewId),
}

#[derive(PartialEq, Eq)]
struct RecordedPatchComment<'a> {
    entity: PatchCommentEntity,
    comment: RecordedComment<'a>,
}

macro_rules! patch_comment_extractor {
    (
        $name:ident,
        $entity_variant:ident,
        $create:ident,
        $edit:ident,
        $redact:ident,
        $entity:ident
    ) => {
        fn $name(action: &PatchAction) -> Option<RecordedPatchComment<'_>> {
            match action {
                PatchAction::$create {
                    $entity,
                    body,
                    embeds,
                    ..
                } => Some(RecordedPatchComment {
                    entity: PatchCommentEntity::$entity_variant(*$entity),
                    comment: RecordedComment::Create(body, embeds),
                }),
                PatchAction::$edit {
                    $entity,
                    comment,
                    body,
                    embeds,
                    ..
                } => Some(RecordedPatchComment {
                    entity: PatchCommentEntity::$entity_variant(*$entity),
                    comment: RecordedComment::Edit(*comment, body, embeds),
                }),
                PatchAction::$redact { $entity, comment } => Some(RecordedPatchComment {
                    entity: PatchCommentEntity::$entity_variant(*$entity),
                    comment: RecordedComment::Redact(*comment),
                }),
                _ => None,
            }
        }
    };
}

patch_comment_extractor!(
    revision_comment,
    Revision,
    RevisionComment,
    RevisionCommentEdit,
    RevisionCommentRedact,
    revision
);
patch_comment_extractor!(
    review_comment,
    Review,
    ReviewComment,
    ReviewCommentEdit,
    ReviewCommentRedact,
    review
);

pub(super) fn review_resolution_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
    review: patch::ReviewId,
    comment: EntryId,
    resolved: bool,
) -> Result<bool, String> {
    Ok(
        find_patch_operation(repository, patch, operation)?.is_some_and(|operation| {
            operation.actions.iter().any(|action| {
                if resolved {
                    matches!(
                        action,
                        PatchAction::ReviewCommentResolve {
                            review: value,
                            comment: candidate,
                        } if *value == review && *candidate == comment
                    )
                } else {
                    matches!(
                        action,
                        PatchAction::ReviewCommentUnresolve {
                            review: value,
                            comment: candidate,
                        } if *value == review && *candidate == comment
                    )
                }
            })
        }),
    )
}

pub(super) fn patch_commit_operation(
    repository: &CobRepository,
    action: &PatchCommitAction,
    operation: EntryId,
    description: &str,
    base: radicle::git::Oid,
    head: radicle::git::Oid,
) -> Result<bool, String> {
    let patch = match action {
        PatchCommitAction::Create { .. } => ObjectId::from(operation),
        PatchCommitAction::Update(patch) => *patch,
    };
    Ok(
        find_patch_operation(repository, patch, operation)?.is_some_and(|operation| {
            operation.actions.iter().any(|action| {
            matches!(
                action,
                PatchAction::Revision {
                    description: value,
                    base: value_base,
                    oid,
                    ..
                } if value == description && *value_base == base && *oid == head
            )
        }) && match action {
            PatchCommitAction::Create {
                title,
                labels,
                draft,
            } => {
                operation.actions.iter().any(|action| {
                    matches!(action, PatchAction::Edit { title: value, .. } if value == title)
                }) && (labels.is_empty()
                    || operation.actions.iter().any(|action| {
                        matches!(
                            action,
                            PatchAction::Label { labels: actual }
                                if actual.iter().eq(labels)
                        )
                    }))
                    && (!draft
                        || operation.actions.iter().any(|action| {
                            matches!(
                                action,
                                PatchAction::Lifecycle {
                                    state: patch::Lifecycle::Draft
                                }
                            )
                        }))
            }
            PatchCommitAction::Update(_) => true,
        }
        }),
    )
}

pub(super) fn merge_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
    revision: patch::RevisionId,
    commit: radicle::git::Oid,
) -> Result<bool, String> {
    Ok(
        find_patch_operation(repository, patch, operation)?.is_some_and(|operation| {
            operation.actions.iter().any(|action| {
                matches!(
                    action,
                    PatchAction::Merge {
                        revision: value,
                        commit: actual,
                    } if *value == revision && *actual == commit
                )
            })
        }),
    )
}

pub(super) fn patch_edit_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
    title: Option<&radicle::cob::Title>,
    revision: Option<patch::RevisionId>,
    description: Option<&str>,
) -> Result<bool, String> {
    edit_operation(
        find_patch_operation(repository, patch, operation),
        title,
        description,
        |action| RecordedEdit::Patch { action, revision },
    )
}

pub(super) fn issue_update_operation(
    repository: &CobRepository,
    issue: ObjectId,
    operation: EntryId,
    update: &IssueUpdate,
) -> Result<bool, String> {
    operation_has_action(
        find_issue_operation(repository, issue, operation),
        |action| issue_update_matches(action, update),
    )
}

pub(super) fn patch_update_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
    update: &PatchUpdate,
) -> Result<bool, String> {
    operation_has_action(
        find_patch_operation(repository, patch, operation),
        |action| patch_update_matches(action, update),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the recovery validator compares every review field"
)]
pub(super) fn review_mutation_operation(
    repository: &CobRepository,
    patch: ObjectId,
    operation: EntryId,
    edit: bool,
    revision: Option<patch::RevisionId>,
    review: Option<patch::ReviewId>,
    verdict: Option<patch::Verdict>,
    summary: Option<&str>,
    labels: &[radicle::cob::Label],
    embeds: &[Embed<Uri>],
) -> Result<bool, String> {
    let recorded = find_patch_operation(repository, patch, operation)?.is_some_and(|operation| {
        operation.actions.iter().any(|action| match action {
            PatchAction::Review {
                revision: actual_revision,
                verdict: actual_verdict,
                summary: actual_summary,
                labels: actual_labels,
            } if !edit => {
                Some(*actual_revision) == revision
                    && *actual_verdict == verdict
                    && actual_summary.as_deref() == summary
                    && actual_labels == labels
            }
            PatchAction::ReviewEdit(actual) if edit => {
                Some(*actual.review_id()) == review
                    && actual.verdict().copied() == verdict
                    && actual.summary().map(String::as_str) == summary
                    && actual.labels() == labels
                    && actual.embeds().is_some_and(|actual| actual == embeds)
            }
            _ => false,
        })
    });
    if !recorded || edit {
        return Ok(recorded);
    }
    Ok(load_patch(repository, patch)?
        .and_then(|patch| find_review(&patch, patch::ReviewId::from(operation)).map(|_| ()))
        .is_some())
}

pub(super) fn find_review(patch: &patch::Patch, id: patch::ReviewId) -> Option<&patch::Review> {
    patch
        .revisions()
        .flat_map(|(_revision, value)| value.reviews())
        .find_map(|(_author, candidate)| (candidate.id() == id).then_some(candidate))
}

fn load_issue(
    repository: &CobRepository,
    subject: ObjectId,
) -> Result<Option<issue::Issue>, String> {
    Issues::open(repository, ReadOnly)
        .map_err(stringify)?
        .get(&subject)
        .map_err(stringify)
}

fn load_patch(
    repository: &CobRepository,
    subject: ObjectId,
) -> Result<Option<patch::Patch>, String> {
    Patches::open(repository, ReadOnly)
        .map_err(stringify)?
        .get(&subject)
        .map_err(stringify)
}

fn edit_operation<A>(
    operation: Result<Option<Op<A>>, String>,
    title: Option<&radicle::cob::Title>,
    description: Option<&str>,
    record: impl Copy + for<'a> Fn(&'a A) -> RecordedEdit<'a>,
) -> Result<bool, String> {
    operation_matches(operation, |operation| {
        let actions = &operation.actions;
        title.is_none_or(|title| {
            actions
                .iter()
                .any(|action| edit_title_matches(record(action), title))
        }) && description.is_none_or(|description| {
            actions
                .iter()
                .any(|action| edit_description_matches(record(action), description))
        })
    })
}

fn edit_title_matches(edit: RecordedEdit<'_>, expected: &radicle::cob::Title) -> bool {
    match edit {
        RecordedEdit::Issue { action, .. } => {
            matches!(action, IssueAction::Edit { title } if title == expected)
        }
        RecordedEdit::Patch { action, .. } => {
            matches!(action, PatchAction::Edit { title, .. } if title == expected)
        }
    }
}

fn edit_description_matches(edit: RecordedEdit<'_>, expected: &str) -> bool {
    match edit {
        RecordedEdit::Issue { action, issue } => {
            matches!(
                action,
                IssueAction::CommentEdit { id, body, .. } if *id == *issue && body == expected
            )
        }
        RecordedEdit::Patch { action, revision } => {
            matches!(
                action,
                PatchAction::RevisionEdit {
                    revision: value,
                    description,
                    ..
                } if Some(*value) == revision && description == expected
            )
        }
    }
}

fn issue_update_matches(action: &IssueAction, update: &IssueUpdate) -> bool {
    match update {
        IssueUpdate::State(expected) => {
            matches!(action, IssueAction::Lifecycle { state } if state == expected)
        }
        IssueUpdate::Labels(expected) => {
            matches!(action, IssueAction::Label { labels } if labels.iter().eq(expected))
        }
        IssueUpdate::Assignees(expected) => {
            matches!(
                action,
                IssueAction::Assign { assignees } if assignees.iter().eq(expected)
            )
        }
    }
}

fn patch_update_matches(action: &PatchAction, update: &PatchUpdate) -> bool {
    match action {
        PatchAction::Lifecycle { state } => {
            matches!(update, PatchUpdate::State(expected) if state == expected)
        }
        PatchAction::Label { labels } => {
            matches!(update, PatchUpdate::Labels(expected) if labels.iter().eq(expected))
        }
        PatchAction::Assign { assignees } => {
            matches!(update, PatchUpdate::Assignees(expected) if assignees == expected)
        }
        _ => false,
    }
}

fn operation_matches<A>(
    operation: Result<Option<Op<A>>, String>,
    matches: impl FnOnce(&Op<A>) -> bool,
) -> Result<bool, String> {
    Ok(operation?.is_some_and(|operation| matches(&operation)))
}

fn operation_has_action<A>(
    operation: Result<Option<Op<A>>, String>,
    matches: impl Fn(&A) -> bool,
) -> Result<bool, String> {
    operation_matches(operation, |operation| operation.actions.iter().any(matches))
}
