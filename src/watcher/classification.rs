//! Shared normalization and member-relevance reduction.
//!
//! Radicle-specific adapters produce [`NormalizedAction`] values. [`Context`]
//! replays them in operation order, tracks member involvement, and emits the
//! first relevant signal for each action.

use std::collections::HashSet;

use radicle::{
    cob::{
        EntryId, Op,
        patch::{ReviewId, RevisionId},
    },
    crypto::PublicKey,
    identity::{Did, RepoId},
};

use crate::{
    conversation::{AdmissionError, Author, Triggers, Turn},
    manifest::Limits,
    member::Member,
    watcher::{
        Change, Comment, Subject, Target,
        activity::{Activity, Delivery, Kind},
    },
};

/// Stateful reducer which classifies normalized actions for one collaborative object.
#[derive(Debug, Default)]
pub(super) struct Context {
    assigned: bool,
    owned: bool,
    seen: bool,
    draft: bool,
    entities: HashSet<Entity>,
    review: Option<OwnReview>,
    revision: Option<RevisionId>,
    turn: Turn,
    ignored: Vec<(Activity, AdmissionError)>,
}

impl Context {
    // Starts a subject's history under the bounds its repository sets.
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            turn: Turn::new(limits),
            ..Self::default()
        }
    }

    pub(super) const fn assigned(&self) -> bool {
        self.assigned
    }

    /// Classifies one operation, emitting activities only for authors allowed to trigger work.
    ///
    /// Reduction runs for every author so that involvement, ownership, and assignment stay
    /// consistent with the repository. Only emission is gated, because suppressing the state
    /// updates would make a later trusted operation on the same subject classify against a
    /// history the repository does not have.
    pub(super) fn classify<A>(
        &mut self,
        operation: &Op<A>,
        member: Member,
        member_id: PublicKey,
        repository: RepoId,
        subject: Subject,
        triggers: &Triggers,
    ) -> Vec<Activity>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let author = Did::from(operation.author);
        let origin = triggers.author(author, Did::from(member_id));
        if !self.seen {
            self.owned = origin == Author::Member;
            self.seen = true;
        }

        let actions: Vec<NormalizedAction<'_>> = operation
            .actions
            .iter()
            .map(|action| ActionEnvelope::new(action, operation, member_id).into())
            .collect();
        // Reactions are acknowledgements, not turns. They must not split consecutive messages
        // from the same peer and spend another exchange when that peer next speaks.
        if actions.iter().any(|action| {
            action.assignment.is_some()
                || action.track.is_some()
                || action.signals.iter().any(|signal| {
                    !matches!(signal,
                Signal::Change { change, .. } if change.is_reaction())
                })
        }) {
            self.turn.begin(author);
        }
        // Lifecycle and revision actions can share one transaction. Apply its final draft state
        // before emitting any signal, including the initial empty revision of a draft patch.
        if let Some(draft) = actions.iter().rev().find_map(|action| action.draft) {
            self.draft = draft;
        }
        let mut activities = Vec::new();
        for (action_index, normalized) in actions.into_iter().enumerate() {
            if let Relevance::Relevant(kind) = self.reduce(normalized, member, origin) {
                if let Err(reason) = self.turn.admit(author, origin) {
                    self.ignored.push((
                        Activity::new(
                            repository,
                            subject,
                            operation.id,
                            action_index,
                            author,
                            kind,
                        ),
                        reason,
                    ));
                    continue;
                }
                tracing::debug!(
                    %author,
                    %member,
                    ?subject,
                    ?kind,
                    operation = %operation.id,
                    "classified activity as relevant"
                );
                activities.push(Activity::new(
                    repository,
                    subject,
                    operation.id,
                    action_index,
                    author,
                    kind,
                ));
            }
        }
        activities
    }

    pub(super) fn take_ignored(&mut self) -> Vec<(Activity, AdmissionError)> {
        std::mem::take(&mut self.ignored)
    }

    /// Reduces one action, narrowing what a member may trigger in another member.
    ///
    /// A non-member is classified as before. A peer member reaches this one only by assigning it
    /// work, naming it directly, addressing its own comments or reviews, or submitting work
    /// against something it owns, which are the deliberate acts a human contributor would perform.
    /// `@team` is excluded so that one member cannot summon the whole cast, and every other
    /// ambient change is excluded so that a thread cannot sustain itself. Reactions never reach
    /// anyone, because agreement is too ambiguous to act on.
    fn reduce(
        &mut self,
        action: NormalizedAction<'_>,
        member: Member,
        origin: Author,
    ) -> Relevance {
        let authored_by_member = origin == Author::Member;
        if let Some(Entity::Revision(revision)) = action.track {
            self.revision = Some(revision);
        }
        if let Some(update) = action.review {
            self.observe_review(update, authored_by_member);
        }
        if authored_by_member && let Some(entity) = action.track {
            self.entities.insert(entity);
        }

        if let Some(assigned) = action.assignment {
            let changed = assigned != self.assigned;
            self.assigned = assigned;
            if changed && !authored_by_member {
                return Relevance::Relevant(if assigned {
                    Kind::Assigned
                } else {
                    Kind::Delivered(Delivery::Changed(Change::Unassigned))
                });
            }
        }

        if authored_by_member {
            return Relevance::Ignored;
        }

        for signal in action.signals {
            match signal {
                Signal::Mention { body, target } if origin.addresses(member, body) => {
                    return Relevance::Relevant(Kind::Delivered(Delivery::Mentioned(target)));
                }
                Signal::Reply {
                    parent: Some(parent),
                    comment,
                } if self.owns(Entity::Comment(parent)) => {
                    return Relevance::Relevant(Kind::Delivered(Delivery::Replied {
                        comment,
                        reply_to: parent,
                    }));
                }
                Signal::Change { audience, change }
                    if !change.is_reaction()
                        && !(self.draft && change.requires_ready_patch())
                        && audience.matches(self, origin, change) =>
                {
                    let change = if audience.follow_review && !self.involved() {
                        let Some(change) = self.review_followup(change) else {
                            continue;
                        };
                        change
                    } else {
                        change
                    };
                    return Relevance::Relevant(Kind::Delivered(Delivery::Changed(change)));
                }
                Signal::Mention { .. } | Signal::Reply { .. } | Signal::Change { .. } => {}
            }
        }
        Relevance::Ignored
    }

    fn involved(&self) -> bool {
        self.owned || self.assigned
    }

    fn observe_review(&mut self, update: ReviewUpdate, authored_by_member: bool) {
        match update {
            ReviewUpdate::Submitted {
                review,
                revision,
                rejected,
            } if authored_by_member => {
                self.review = Some(OwnReview {
                    id: review,
                    revision,
                    rejected,
                });
            }
            ReviewUpdate::Edited { review, rejected } => {
                if let Some(own) = &mut self.review
                    && own.id == review
                {
                    own.rejected = rejected;
                }
            }
            ReviewUpdate::Redacted(review) => {
                if self.review.as_ref().is_some_and(|own| own.id == review) {
                    self.review = None;
                }
            }
            ReviewUpdate::Submitted { .. } => {}
        }
    }

    fn review_followup(&self, change: Change) -> Option<Change> {
        let review = self.review.as_ref().filter(|review| review.rejected)?;
        let revision = match change {
            Change::RevisionCreated(revision) => revision,
            Change::PatchReady => self.revision?,
            _ => return None,
        };
        (revision != review.revision).then_some(Change::ReviewFollowUp {
            review: review.id,
            revision,
        })
    }

    fn owns(&self, entity: Entity) -> bool {
        self.entities.contains(&entity)
    }
}

#[derive(Debug)]
struct OwnReview {
    id: ReviewId,
    revision: RevisionId,
    rejected: bool,
}

#[derive(Debug)]
pub(super) enum ReviewUpdate {
    Submitted {
        review: ReviewId,
        revision: RevisionId,
        rejected: bool,
    },
    Edited {
        review: ReviewId,
        rejected: bool,
    },
    Redacted(ReviewId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Entity {
    Comment(EntryId),
    Review(ReviewId),
    Revision(RevisionId),
}

/// Radicle-independent representation of one action for relevance processing.
///
/// Issue and patch adapters convert their Radicle actions into this shared form. It describes
/// optional assignment and ownership updates followed by ordered relevance signals. [`Context`]
/// applies those updates and evaluates the signals without depending on Radicle's concrete action
/// enums.
#[derive(Debug, Default)]
pub(super) struct NormalizedAction<'a> {
    assignment: Option<bool>,
    draft: Option<bool>,
    track: Option<Entity>,
    review: Option<ReviewUpdate>,
    signals: Vec<Signal<'a>>,
}

impl<'a> NormalizedAction<'a> {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn assignment(mut self, assigned: bool) -> Self {
        self.assignment = Some(assigned);
        self
    }

    pub(super) fn draft(mut self, draft: bool) -> Self {
        self.draft = Some(draft);
        self
    }

    pub(super) fn track(mut self, entity: Entity) -> Self {
        self.track = Some(entity);
        self
    }

    pub(super) fn review(mut self, update: ReviewUpdate) -> Self {
        self.review = Some(update);
        self
    }

    pub(super) fn mention(mut self, body: &'a str, target: Target) -> Self {
        self.signals.push(Signal::Mention { body, target });
        self
    }

    pub(super) fn mention_if(self, body: Option<&'a str>, target: Target) -> Self {
        match body {
            Some(body) => self.mention(body, target),
            None => self,
        }
    }

    pub(super) fn reply(mut self, parent: Option<EntryId>, comment: Comment) -> Self {
        self.signals.push(Signal::Reply { parent, comment });
        self
    }

    pub(super) fn change(mut self, audience: Audience, change: Change) -> Self {
        self.signals.push(Signal::Change { audience, change });
        self
    }

    pub(super) fn resolution(review: ReviewId, comment: EntryId, resolved: bool) -> Self {
        Self::new().change(
            Audience::owned(Entity::Comment(comment)).or_owned(Entity::Review(review)),
            Change::CommentResolution {
                comment: Comment::Review { review, comment },
                resolved,
            },
        )
    }
}

/// Input for converting a Radicle action into a [`NormalizedAction`].
///
/// It bundles the action with its parent operation and the watched member's identity because
/// [`From`] accepts only one input. The generic `A` ensures that the action and its operation
/// always use the same Radicle action type.
pub(super) struct ActionEnvelope<'a, A> {
    action: &'a A,
    operation: &'a Op<A>,
    member_id: PublicKey,
}

impl<'a, A> ActionEnvelope<'a, A> {
    pub(super) const fn new(action: &'a A, operation: &'a Op<A>, member_id: PublicKey) -> Self {
        Self {
            action,
            operation,
            member_id,
        }
    }

    pub(super) fn into_parts(self) -> (&'a A, &'a Op<A>, PublicKey) {
        (self.action, self.operation, self.member_id)
    }
}

#[derive(Debug, Default)]
pub(super) struct Audience {
    involved: bool,
    follow_review: bool,
    owners: Vec<Entity>,
}

impl Audience {
    pub(super) fn involved() -> Self {
        Self {
            involved: true,
            ..Self::default()
        }
    }

    pub(super) fn review_followup() -> Self {
        Self {
            follow_review: true,
            ..Self::involved()
        }
    }

    pub(super) fn owned(entity: Entity) -> Self {
        Self {
            owners: vec![entity],
            ..Self::default()
        }
    }

    pub(super) fn involved_or(entity: Entity) -> Self {
        Self::involved().or_owned(entity)
    }

    pub(super) fn owned_by_any(entities: impl IntoIterator<Item = Entity>) -> Self {
        Self {
            owners: entities.into_iter().collect(),
            ..Self::default()
        }
    }

    pub(super) fn or_owned(mut self, entity: Entity) -> Self {
        self.owners.push(entity);
        self
    }

    fn matches(&self, context: &Context, origin: Author, change: Change) -> bool {
        self.owners.iter().any(|entity| context.owns(*entity))
            || ((origin != Author::Peer || change.is_task_submission())
                && ((self.involved && context.involved())
                    || (self.follow_review
                        && context
                            .review
                            .as_ref()
                            .is_some_and(|review| review.rejected))))
    }
}

#[derive(Debug)]
enum Signal<'a> {
    Mention {
        body: &'a str,
        target: Target,
    },
    Reply {
        parent: Option<EntryId>,
        comment: Comment,
    },
    Change {
        audience: Audience,
        change: Change,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Relevance {
    Ignored,
    Relevant(Kind),
}
