//! Activities which may require the selected member's attention.

use radicle::{
    cob::{
        EntryId, ObjectId,
        patch::{ReviewId, RevisionId},
    },
    identity::{Did, RepoId},
};

use crate::agent::{self, Agent, Context, DeliveryId, Outcome};

/// Activity addressed to or otherwise relevant to the selected member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Activity {
    /// Repository containing the activity.
    pub repository: RepoId,
    /// Issue or patch receiving the activity.
    pub subject: Subject,
    /// Operation which produced the activity.
    pub operation: EntryId,
    /// Position of the relevant action within the operation.
    pub action: usize,
    /// Author of the operation.
    pub author: Did,
    /// Why the activity requires attention.
    pub kind: Kind,
}

impl Activity {
    pub(super) fn new(
        repository: RepoId,
        subject: Subject,
        operation: EntryId,
        action: usize,
        author: Did,
        kind: Kind,
    ) -> Self {
        Self {
            repository,
            subject,
            operation,
            action,
            author,
            kind,
        }
    }

    pub(super) async fn dispatch(
        self,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<Outcome, agent::Error> {
        let context = Context {
            repository: self.repository,
            subject: self.subject,
            delivery: self.delivery_id(),
            author: self.author,
        };
        match self.kind {
            Kind::Assigned => agent.assigned(context).await,
            Kind::Mentioned(target) => agent.mentioned(context, target).await,
            Kind::Replied { comment, reply_to } => agent.replied(context, comment, reply_to).await,
            Kind::Changed(change) => agent.changed(context, change).await,
        }
    }

    pub(super) const fn delivery_id(&self) -> DeliveryId {
        DeliveryId {
            operation: self.operation,
            action: self.action,
        }
    }
}

/// Collaborative object receiving an interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// Radicle issue.
    Issue(ObjectId),
    /// Radicle patch.
    Patch(ObjectId),
}

/// Reason an interaction requires the member's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// The member was assigned an issue or patch review.
    Assigned,
    /// The member was mentioned in text.
    Mentioned(Target),
    /// A comment replied to one of the member's comments.
    Replied {
        /// New reply.
        comment: Comment,
        /// Member comment receiving the reply.
        reply_to: EntryId,
    },
    /// Another relevant interaction occurred.
    Changed(Change),
}

/// Location containing a mention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Issue or patch title or description.
    Subject,
    /// Discussion comment.
    Comment(Comment),
    /// Patch review summary.
    Review(ReviewId),
    /// Patch revision description.
    Revision(RevisionId),
}

/// Comment location within a collaborative object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comment {
    /// Issue discussion comment.
    Issue(EntryId),
    /// Comment on a patch revision.
    Revision {
        /// Revision containing the comment.
        revision: RevisionId,
        /// Comment identifier.
        comment: EntryId,
    },
    /// Comment within a patch review.
    Review {
        /// Review containing the comment.
        review: ReviewId,
        /// Comment identifier.
        comment: EntryId,
    },
}

impl Comment {
    pub(super) const fn revision(revision: RevisionId, comment: EntryId) -> Self {
        Self::Revision { revision, comment }
    }
}

/// Change relevant because the member owns, reviews, or is assigned to the subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The member was removed from the assignee set.
    Unassigned,
    /// The subject title, target, or other primary metadata changed.
    Edited,
    /// The subject labels changed.
    LabelsChanged,
    /// The issue or patch lifecycle changed.
    LifecycleChanged,
    /// A patch was merged.
    Merged,
    /// A comment was created.
    CommentCreated(Comment),
    /// A comment was edited.
    CommentEdited(Comment),
    /// A comment was redacted.
    CommentRedacted(Comment),
    /// A reaction on a comment changed.
    CommentReaction {
        /// Comment receiving the reaction.
        comment: Comment,
        /// Whether the reaction was added or removed.
        active: bool,
    },
    /// A review comment was resolved or reopened.
    CommentResolution {
        /// Comment whose resolution changed.
        comment: Comment,
        /// Whether the comment is now resolved.
        resolved: bool,
    },
    /// A revision resolved one or more review comments by the member.
    ReviewCommentsResolved,
    /// A review was submitted.
    ReviewSubmitted {
        /// Review identifier.
        review: ReviewId,
        /// Reviewed revision.
        revision: RevisionId,
    },
    /// A review was edited.
    ReviewEdited(ReviewId),
    /// A review was redacted.
    ReviewRedacted(ReviewId),
    /// A reaction on a review changed.
    ReviewReaction {
        /// Review receiving the reaction.
        review: ReviewId,
        /// Whether the reaction was added or removed.
        active: bool,
    },
    /// A patch revision was created.
    RevisionCreated(RevisionId),
    /// A patch revision was edited.
    RevisionEdited(RevisionId),
    /// A patch revision was redacted.
    RevisionRedacted(RevisionId),
    /// A reaction on a revision changed.
    RevisionReaction {
        /// Revision receiving the reaction.
        revision: RevisionId,
        /// Whether the reaction was added or removed.
        active: bool,
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        cob::{EntryId, ObjectId},
        identity::{Did, RepoId},
    };

    use crate::{
        agent::{Agent, Context, DeliveryId, Error, Outcome},
        watcher::activity::{Activity, Change, Comment, Kind, Subject, Target},
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";
    const OPERATION: &str = "2222222222222222222222222222222222222222";
    const COMMENT: &str = "3333333333333333333333333333333333333333";
    const AUTHOR: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Assigned(Context),
        Mentioned(Context, Target),
        Replied(Context, Comment, EntryId),
        Changed(Context, Change),
    }

    #[derive(Debug, Default)]
    struct RecordingAgent {
        calls: Vec<Call>,
    }

    #[async_trait::async_trait]
    impl Agent for RecordingAgent {
        async fn assigned(&mut self, context: Context) -> Result<Outcome, Error> {
            self.calls.push(Call::Assigned(context));
            Ok(Outcome::NoAction)
        }

        async fn mentioned(&mut self, context: Context, target: Target) -> Result<Outcome, Error> {
            self.calls.push(Call::Mentioned(context, target));
            Ok(Outcome::NoAction)
        }

        async fn replied(
            &mut self,
            context: Context,
            comment: Comment,
            reply_to: EntryId,
        ) -> Result<Outcome, Error> {
            self.calls.push(Call::Replied(context, comment, reply_to));
            Ok(Outcome::NoAction)
        }

        async fn changed(&mut self, context: Context, change: Change) -> Result<Outcome, Error> {
            self.calls.push(Call::Changed(context, change));
            Ok(Outcome::NoAction)
        }
    }

    fn context() -> Context {
        Context {
            repository: RepoId::from_str(REPOSITORY).expect("repository should be valid"),
            subject: Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should be valid")),
            delivery: DeliveryId {
                operation: EntryId::from_str(OPERATION).expect("operation should be valid"),
                action: 0,
            },
            author: Did::from_str(AUTHOR).expect("author should be valid"),
        }
    }

    fn activity(kind: Kind) -> Activity {
        let context = context();
        Activity::new(
            context.repository,
            context.subject,
            context.delivery.operation,
            context.delivery.action,
            context.author,
            kind,
        )
    }

    #[tokio::test]
    async fn dispatches_every_activity_to_its_typed_agent_method() {
        let comment =
            Comment::Issue(EntryId::from_str(COMMENT).expect("comment operation should be valid"));
        let mut agent = RecordingAgent::default();

        activity(Kind::Assigned)
            .dispatch(&mut agent)
            .await
            .expect("assignment should dispatch");
        activity(Kind::Mentioned(Target::Comment(comment)))
            .dispatch(&mut agent)
            .await
            .expect("mention should dispatch");
        activity(Kind::Replied {
            comment,
            reply_to: context().delivery.operation,
        })
        .dispatch(&mut agent)
        .await
        .expect("reply should dispatch");
        activity(Kind::Changed(Change::CommentCreated(comment)))
            .dispatch(&mut agent)
            .await
            .expect("change should dispatch");

        assert_eq!(
            agent.calls,
            [
                Call::Assigned(context()),
                Call::Mentioned(context(), Target::Comment(comment)),
                Call::Replied(context(), comment, context().delivery.operation),
                Call::Changed(context(), Change::CommentCreated(comment)),
            ]
        );
    }
}
