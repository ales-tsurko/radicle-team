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
    identity::{Did, RepoId},
    node::NodeId,
};

use crate::{
    manifest::TriggerAccess,
    member::Member,
    watcher::{
        Change, Comment, Subject, Target,
        activity::{Activity, Delivery, Kind},
    },
};

/// Responses one member may publish on a subject before a non-member acts again.
///
/// Members govern themselves rather than sharing a counter, because each runs its own process and
/// may run on its own host. The bound is therefore this limit multiplied by the fixed cast size.
const RESPONSE_LIMIT: usize = 3;

/// Repository trigger policy resolved against the current delegate set.
#[derive(Debug, Clone)]
pub(super) struct Triggers {
    access: TriggerAccess,
    delegates: Vec<Did>,
    members: Vec<Did>,
}

impl Triggers {
    pub(super) const fn new(access: TriggerAccess, delegates: Vec<Did>, members: Vec<Did>) -> Self {
        Self {
            access,
            delegates,
            members,
        }
    }

    /// Returns whether `author` may put this repository's members to work.
    ///
    /// Members are always allowed, whatever `triggered_by` says: that setting keeps outside
    /// identities from spending the operator's budget, and the repository's own members are not
    /// outside identities. What one member may trigger in another is narrowed separately, when
    /// the operation's signals are evaluated.
    pub(super) fn allows(&self, author: Did) -> bool {
        self.is_member(author) || self.access.allows(author, &self.delegates)
    }

    pub(super) fn is_member(&self, author: Did) -> bool {
        self.members.contains(&author)
    }
}

/// Stateful reducer which classifies normalized actions for one collaborative object.
#[derive(Debug, Default)]
pub(super) struct Context {
    assigned: bool,
    owned: bool,
    seen: bool,
    entities: HashSet<Entity>,
    responses: usize,
}

impl Context {
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
        member_id: NodeId,
        repository: RepoId,
        subject: Subject,
        triggers: &Triggers,
    ) -> Vec<Activity>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let authored_by_member = operation.author == member_id;
        if !self.seen {
            self.owned = authored_by_member;
            self.seen = true;
        }
        let author = Did::from(operation.author);
        let authored_by_peer = !authored_by_member && triggers.is_member(author);
        let may_trigger = authored_by_member || triggers.allows(author);

        if !triggers.is_member(author) {
            self.responses = 0;
        }
        let exhausted = self.responses >= RESPONSE_LIMIT;

        let mut activities = Vec::new();
        for (action_index, action) in operation.actions.iter().enumerate() {
            let normalized = ActionEnvelope::new(action, operation, member_id).into();
            if let Relevance::Relevant(kind) =
                self.reduce(normalized, member, authored_by_member, authored_by_peer)
            {
                if exhausted {
                    tracing::info!(
                        %author,
                        %member,
                        ?subject,
                        ?kind,
                        operation = %operation.id,
                        limit = RESPONSE_LIMIT,
                        "ignored relevant activity because the member already answered this \
                         subject its allowed number of times since a non-member acted"
                    );
                    continue;
                }
                if !may_trigger {
                    tracing::info!(
                        %author,
                        %member,
                        ?subject,
                        ?kind,
                        operation = %operation.id,
                        "ignored relevant activity because its author may not trigger agents"
                    );
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

    /// Reduces one action, narrowing what a member may trigger in another member.
    ///
    /// A non-member is classified as before. A peer member reaches this one only by assigning it
    /// work, naming it directly, replying to a comment it owns, or submitting work against
    /// something it owns, which are the deliberate acts a human contributor would perform.
    /// `@team` is excluded so that one member cannot summon the whole cast, and every other
    /// ambient change is excluded so that a thread cannot sustain itself. Reactions never reach
    /// anyone, because agreement is too ambiguous to act on.
    fn reduce(
        &mut self,
        action: NormalizedAction<'_>,
        member: Member,
        authored_by_member: bool,
        authored_by_peer: bool,
    ) -> Relevance {
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
            if action.signals.iter().any(Signal::writes_comment) {
                self.responses = self.responses.saturating_add(1);
            }
            return Relevance::Ignored;
        }

        let addresses = |body: &str| {
            if authored_by_peer {
                member.is_named_in(body)
            } else {
                member.is_mentioned_in(body)
            }
        };

        for signal in action.signals {
            match signal {
                Signal::Mention { body, target } if addresses(body) => {
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
                        && (!authored_by_peer || change.is_work_submission())
                        && audience.matches(self) =>
                {
                    return Relevance::Relevant(Kind::Delivered(Delivery::Changed(change)));
                }
                Signal::Mention { .. } | Signal::Reply { .. } | Signal::Change { .. } => {}
            }
        }
        Relevance::Ignored
    }

    fn involved(&self) -> bool {
        self.owned
            || self.assigned
            || self
                .entities
                .iter()
                .any(|entity| matches!(entity, Entity::Review(_)))
    }

    fn owns(&self, entity: Entity) -> bool {
        self.entities.contains(&entity)
    }
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
    track: Option<Entity>,
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

    pub(super) fn track(mut self, entity: Entity) -> Self {
        self.track = Some(entity);
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
    member_id: NodeId,
}

impl<'a, A> ActionEnvelope<'a, A> {
    pub(super) const fn new(action: &'a A, operation: &'a Op<A>, member_id: NodeId) -> Self {
        Self {
            action,
            operation,
            member_id,
        }
    }

    pub(super) fn into_parts(self) -> (&'a A, &'a Op<A>, NodeId) {
        (self.action, self.operation, self.member_id)
    }
}

#[derive(Debug, Default)]
pub(super) struct Audience {
    involved: bool,
    owners: Vec<Entity>,
}

impl Audience {
    pub(super) fn involved() -> Self {
        Self {
            involved: true,
            owners: Vec::new(),
        }
    }

    pub(super) fn owned(entity: Entity) -> Self {
        Self {
            involved: false,
            owners: vec![entity],
        }
    }

    pub(super) fn involved_or(entity: Entity) -> Self {
        Self::involved().or_owned(entity)
    }

    pub(super) fn owned_by_any(entities: impl IntoIterator<Item = Entity>) -> Self {
        Self {
            involved: false,
            owners: entities.into_iter().collect(),
        }
    }

    pub(super) fn or_owned(mut self, entity: Entity) -> Self {
        self.owners.push(entity);
        self
    }

    fn matches(&self, context: &Context) -> bool {
        (self.involved && context.involved())
            || self.owners.iter().any(|entity| context.owns(*entity))
    }
}

impl Signal<'_> {
    // Returns whether this signal is the member speaking on the subject.
    fn writes_comment(&self) -> bool {
        matches!(
            self,
            Self::Change {
                change: Change::CommentCreated(Comment::Issue(_) | Comment::Revision { .. }),
                ..
            }
        )
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
