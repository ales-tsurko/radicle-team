//! Conversion from Radicle issue actions into the shared watcher action model.

use radicle::cob::issue::Action;

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
            Action::Edit { title } => NormalizedAction::new()
                .mention(title.as_ref(), Target::Subject)
                .change(Audience::involved(), Change::Edited),
            Action::Lifecycle { .. } => {
                NormalizedAction::new().change(Audience::involved(), Change::LifecycleChanged)
            }
            Action::Label { .. } => {
                NormalizedAction::new().change(Audience::involved(), Change::LabelsChanged)
            }
            Action::Comment { body, reply_to, .. } => {
                let comment = Comment::Issue(operation.id);
                NormalizedAction::new()
                    .track(Entity::Comment(operation.id))
                    .mention(body, Target::Comment(comment))
                    .reply(*reply_to, comment)
                    .change(Audience::involved(), Change::CommentCreated(comment))
            }
            Action::CommentEdit { id, body, .. } => {
                let comment = Comment::Issue(*id);
                NormalizedAction::new()
                    .mention(body, Target::Comment(comment))
                    .change(Audience::involved(), Change::CommentEdited(comment))
            }
            Action::CommentRedact { id } => {
                let comment = Comment::Issue(*id);
                NormalizedAction::new().change(
                    Audience::involved_or(Entity::Comment(*id)),
                    Change::CommentRedacted(comment),
                )
            }
            Action::CommentReact { id, active, .. } => {
                let comment = Comment::Issue(*id);
                NormalizedAction::new().change(
                    Audience::owned(Entity::Comment(*id)),
                    Change::CommentReaction {
                        comment,
                        active: *active,
                    },
                )
            }
        }
    }
}
