//! Validation of journaled mutations during replay.

use radicle::cob::{
    EntryId, ObjectId, Op,
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
}

type CobRepository = radicle::storage::git::Repository;
type CommentBody = Result<Option<String>, String>;

pub(super) fn comment_applied(
    repository: &CobRepository,
    recovery: CommentRecovery<'_>,
) -> Result<bool, String> {
    let actual = match (recovery.kind, recovery.target) {
        (SubjectKind::Issue, CommentTargetKind::Issue) => issue_body(repository, &recovery),
        (SubjectKind::Patch, CommentTargetKind::Revision) => revision_body(repository, &recovery),
        (SubjectKind::Patch, CommentTargetKind::Review) => review_body(repository, &recovery),
        _ => return Ok(false),
    };
    actual.map(|actual| comment_body_applied(recovery.operation, recovery.body, actual.as_deref()))
}

pub(super) fn comment_operation(
    repository: &CobRepository,
    operation: EntryId,
    recovery: CommentRecovery<'_>,
) -> Result<bool, String> {
    match (recovery.kind, recovery.target) {
        (SubjectKind::Issue, CommentTargetKind::Issue) => {
            let expected = recorded_comment(&recovery);
            Ok(
                find_issue_operation(repository, recovery.subject, operation)?.is_some_and(
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
        (SubjectKind::Patch, CommentTargetKind::Revision) => patch_comment_operation(
            repository,
            operation,
            &recovery,
            PatchCommentTarget::Revision,
        ),
        (SubjectKind::Patch, CommentTargetKind::Review) => {
            patch_comment_operation(repository, operation, &recovery, PatchCommentTarget::Review)
        }
        _ => Ok(false),
    }
}

#[derive(PartialEq, Eq)]
enum RecordedComment<'a> {
    Create(&'a str),
    Edit(EntryId, &'a str),
    Redact(EntryId),
}

fn recorded_comment<'a>(recovery: &'a CommentRecovery<'a>) -> RecordedComment<'a> {
    match recovery.operation {
        CommentOperation::Create => {
            RecordedComment::Create(recovery.body.expect("body was validated"))
        }
        CommentOperation::Edit => {
            RecordedComment::Edit(recovery.comment, recovery.body.expect("body was validated"))
        }
        CommentOperation::Redact => RecordedComment::Redact(recovery.comment),
    }
}

fn issue_comment(action: &IssueAction) -> Option<RecordedComment<'_>> {
    match action {
        IssueAction::Comment { body, .. } => Some(RecordedComment::Create(body)),
        IssueAction::CommentEdit { id, body, .. } => Some(RecordedComment::Edit(*id, body)),
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

fn patch_comment_operation<'a>(
    repository: &CobRepository,
    operation: EntryId,
    recovery: &'a CommentRecovery<'a>,
    target: PatchCommentTarget,
) -> Result<bool, String> {
    let entity = recovery
        .entity
        .ok_or_else(|| "comment entity is required".to_owned())?;
    let expected = RecordedPatchComment {
        entity: match target {
            PatchCommentTarget::Revision => {
                PatchCommentEntity::Revision(patch::RevisionId::from(entity))
            }
            PatchCommentTarget::Review => PatchCommentEntity::Review(patch::ReviewId::from(entity)),
        },
        comment: recorded_comment(recovery),
    };
    Ok(
        find_patch_operation(repository, recovery.subject, operation)?.is_some_and(|operation| {
            operation
                .actions
                .iter()
                .filter_map(|action| revision_comment(action).or_else(|| review_comment(action)))
                .any(|actual| actual == expected)
        }),
    )
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
                PatchAction::$create { $entity, body, .. } => Some(RecordedPatchComment {
                    entity: PatchCommentEntity::$entity_variant(*$entity),
                    comment: RecordedComment::Create(body),
                }),
                PatchAction::$edit {
                    $entity,
                    comment,
                    body,
                    ..
                } => Some(RecordedPatchComment {
                    entity: PatchCommentEntity::$entity_variant(*$entity),
                    comment: RecordedComment::Edit(*comment, body),
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

fn comment_body_applied(
    operation: CommentOperation,
    expected: Option<&str>,
    actual: Option<&str>,
) -> bool {
    match operation {
        CommentOperation::Create | CommentOperation::Edit => {
            actual.is_some_and(|actual| actual == expected.expect("body was validated"))
        }
        CommentOperation::Redact => actual.is_none(),
    }
}

fn issue_body(repository: &CobRepository, recovery: &CommentRecovery<'_>) -> CommentBody {
    Ok(load_issue(repository, recovery.subject)?.and_then(|issue| {
        issue
            .thread()
            .comment(&recovery.comment)
            .map(|value| value.body().to_owned())
    }))
}

fn revision_body(repository: &CobRepository, recovery: &CommentRecovery<'_>) -> CommentBody {
    let revision = recovery
        .entity
        .ok_or_else(|| "entity revision is required".to_owned())?;
    Ok(load_patch(repository, recovery.subject)?.and_then(|patch| {
        patch
            .revision(&patch::RevisionId::from(revision))
            .and_then(|value| value.discussion().comment(&recovery.comment))
            .map(|value| value.body().to_owned())
    }))
}

fn review_body(repository: &CobRepository, recovery: &CommentRecovery<'_>) -> CommentBody {
    let review = recovery
        .entity
        .ok_or_else(|| "entity review is required".to_owned())?;
    Ok(load_patch(repository, recovery.subject)?.and_then(|patch| {
        find_review(&patch, patch::ReviewId::from(review)).and_then(|review| {
            review
                .comments()
                .find(|(candidate, _value)| **candidate == recovery.comment)
                .map(|(_candidate, value)| value.body().to_owned())
        })
    }))
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
