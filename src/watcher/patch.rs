//! Conversion from Radicle patch actions into the shared watcher action model.

use radicle::cob::patch::{Action, Lifecycle, ReviewId, RevisionId};

use crate::watcher::{
    Change, Comment, Target,
    classification::{ActionEnvelope, Audience, Entity, NormalizedAction},
};

impl<'a> From<ActionEnvelope<'a, Action>> for NormalizedAction<'a> {
    fn from(envelope: ActionEnvelope<'a, Action>) -> Self {
        let (action, operation, member_id) = envelope.into_parts();
        match action {
            Action::Assign { assignees } => {
                NormalizedAction::new().assignment(assignees.contains(&member_id.into()))
            }
            Action::Edit { title, .. } => NormalizedAction::new()
                .mention(title.as_ref(), Target::Subject)
                .change(Audience::involved(), Change::Edited),
            Action::Label { .. } => {
                NormalizedAction::new().change(Audience::involved(), Change::LabelsChanged)
            }
            Action::Lifecycle {
                state: Lifecycle::Open,
            } => NormalizedAction::new()
                .draft(false)
                .change(Audience::involved(), Change::PatchReady),
            Action::Lifecycle { state } => NormalizedAction::new()
                .draft(matches!(state, Lifecycle::Draft))
                .change(Audience::involved(), Change::LifecycleChanged),
            Action::Merge { .. } => {
                NormalizedAction::new().change(Audience::involved(), Change::Merged)
            }
            Action::Review {
                revision, summary, ..
            } => {
                let review = ReviewId::from(operation.id);
                NormalizedAction::new()
                    .track(Entity::Review(review))
                    .mention_if(summary.as_deref(), Target::Review(review))
                    .change(
                        Audience::involved(),
                        Change::ReviewSubmitted {
                            review,
                            revision: *revision,
                        },
                    )
            }
            Action::ReviewEdit(edit) => {
                let review = *edit.review_id();
                NormalizedAction::new()
                    .mention_if(edit.summary().map(String::as_str), Target::Review(review))
                    .change(
                        Audience::involved_or(Entity::Review(review)),
                        Change::ReviewEdited(review),
                    )
            }
            Action::ReviewRedact { review } => NormalizedAction::new().change(
                Audience::involved_or(Entity::Review(*review)),
                Change::ReviewRedacted(*review),
            ),
            Action::ReviewReact { review, active, .. } => NormalizedAction::new().change(
                Audience::owned(Entity::Review(*review)),
                Change::ReviewReaction {
                    review: *review,
                    active: *active,
                },
            ),
            Action::ReviewComment {
                review,
                body,
                reply_to,
                ..
            } => {
                let comment = Comment::Review {
                    review: *review,
                    comment: operation.id,
                };
                NormalizedAction::new()
                    .track(Entity::Comment(operation.id))
                    .mention(body, Target::Comment(comment))
                    .reply(*reply_to, comment)
                    .change(
                        Audience::involved_or(Entity::Review(*review)),
                        Change::CommentCreated(comment),
                    )
            }
            Action::ReviewCommentEdit {
                review,
                comment,
                body,
                ..
            } => {
                let target = Comment::Review {
                    review: *review,
                    comment: *comment,
                };
                NormalizedAction::new()
                    .mention(body, Target::Comment(target))
                    .change(
                        Audience::involved_or(Entity::Review(*review)),
                        Change::CommentEdited(target),
                    )
            }
            Action::ReviewCommentRedact { review, comment } => {
                let target = Comment::Review {
                    review: *review,
                    comment: *comment,
                };
                NormalizedAction::new().change(
                    Audience::involved_or(Entity::Review(*review))
                        .or_owned(Entity::Comment(*comment)),
                    Change::CommentRedacted(target),
                )
            }
            Action::ReviewCommentReact {
                review,
                comment,
                active,
                ..
            } => NormalizedAction::new().change(
                Audience::owned(Entity::Comment(*comment)),
                Change::CommentReaction {
                    comment: Comment::Review {
                        review: *review,
                        comment: *comment,
                    },
                    active: *active,
                },
            ),
            Action::ReviewCommentResolve { review, comment } => {
                NormalizedAction::resolution(*review, *comment, true)
            }
            Action::ReviewCommentUnresolve { review, comment } => {
                NormalizedAction::resolution(*review, *comment, false)
            }
            Action::Revision {
                description,
                resolves,
                ..
            } => {
                let revision = RevisionId::from(operation.id);
                NormalizedAction::new()
                    .track(Entity::Revision(revision))
                    .mention(description, Target::Revision(revision))
                    .change(
                        Audience::owned_by_any(
                            resolves
                                .iter()
                                .map(|(_, comment)| Entity::Comment(*comment)),
                        ),
                        Change::ReviewCommentsResolved,
                    )
                    .change(Audience::involved(), Change::RevisionCreated(revision))
            }
            Action::RevisionEdit {
                revision,
                description,
                ..
            } => NormalizedAction::new()
                .mention(description, Target::Revision(*revision))
                .change(Audience::involved(), Change::RevisionEdited(*revision)),
            Action::RevisionRedact { revision } => NormalizedAction::new().change(
                Audience::involved_or(Entity::Revision(*revision)),
                Change::RevisionRedacted(*revision),
            ),
            Action::RevisionReact {
                revision, active, ..
            } => NormalizedAction::new().change(
                Audience::owned(Entity::Revision(*revision)),
                Change::RevisionReaction {
                    revision: *revision,
                    active: *active,
                },
            ),
            Action::RevisionComment {
                revision,
                body,
                reply_to,
                ..
            } => {
                let comment = Comment::revision(*revision, operation.id);
                NormalizedAction::new()
                    .track(Entity::Comment(operation.id))
                    .mention(body, Target::Comment(comment))
                    .reply(*reply_to, comment)
                    .change(Audience::involved(), Change::CommentCreated(comment))
            }
            Action::RevisionCommentEdit {
                revision,
                comment,
                body,
                ..
            } => {
                let target = Comment::revision(*revision, *comment);
                NormalizedAction::new()
                    .mention(body, Target::Comment(target))
                    .change(Audience::involved(), Change::CommentEdited(target))
            }
            Action::RevisionCommentRedact { revision, comment } => {
                let target = Comment::revision(*revision, *comment);
                NormalizedAction::new().change(
                    Audience::involved_or(Entity::Comment(*comment)),
                    Change::CommentRedacted(target),
                )
            }
            Action::RevisionCommentReact {
                revision,
                comment,
                active,
                ..
            } => NormalizedAction::new().change(
                Audience::owned(Entity::Comment(*comment)),
                Change::CommentReaction {
                    comment: Comment::revision(*revision, *comment),
                    active: *active,
                },
            ),
        }
    }
}
