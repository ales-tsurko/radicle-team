//! Activities which may require the selected member's attention.

use std::{fmt, path::PathBuf, str::FromStr};

use radicle::{
    cob::{
        EntryId, ObjectId,
        patch::{ReviewId, RevisionId},
    },
    identity::{Did, RepoId},
};

use crate::agent::{self, Context, DeliveryId, Talker};

/// Activity addressed to or otherwise relevant to the selected member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activity {
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

    pub(crate) fn context(&self, workspace: PathBuf) -> Context {
        Context {
            repository: self.repository,
            subject: self.subject,
            delivery: self.delivery_id(),
            author: self.author,
            workspace,
        }
    }

    pub(crate) const fn delivery_id(&self) -> DeliveryId {
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

impl Subject {
    /// Returns the identifier of the issue or patch.
    #[must_use]
    pub const fn id(&self) -> ObjectId {
        match self {
            Self::Issue(id) | Self::Patch(id) => *id,
        }
    }

    /// Returns the word for this kind of subject.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Issue(_) => "issue",
            Self::Patch(_) => "patch",
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind(), self.id())
    }
}

impl FromStr for Subject {
    type Err = InvalidSubject;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidSubject(value.to_owned());
        let (kind, id) = value.split_once(':').ok_or_else(invalid)?;
        let id = ObjectId::from_str(id).map_err(|_error| invalid())?;
        match kind {
            "issue" => Ok(Self::Issue(id)),
            "patch" => Ok(Self::Patch(id)),
            _ => Err(invalid()),
        }
    }
}

/// A stored subject address could not be read back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid subject '{0}'")]
pub struct InvalidSubject(String);

// Reason an interaction requires the member's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Assigned,
    // Something happened that the talking half should answer.
    Delivered(Delivery),
}

// An interaction the talking half handles itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Mentioned(Target),
    Replied { comment: Comment, reply_to: EntryId },
    Changed(Change),
}

impl Delivery {
    pub(crate) async fn dispatch(
        self,
        context: Context,
        agent: &mut (impl Talker + ?Sized),
    ) -> Result<(), agent::Error> {
        match self {
            Self::Mentioned(target) => agent.mentioned(context, target).await,
            Self::Replied { comment, reply_to } => agent.replied(context, comment, reply_to).await,
            Self::Changed(change) => agent.changed(context, change).await,
        }
    }
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

impl Change {
    /// Returns whether this change delivers work to whoever owns the subject.
    #[must_use]
    pub const fn is_work_submission(&self) -> bool {
        matches!(
            self,
            Self::ReviewSubmitted { .. } | Self::RevisionCreated(_)
        )
    }

    /// Returns whether this change is a reaction being added or removed.
    #[must_use]
    pub const fn is_reaction(&self) -> bool {
        matches!(
            self,
            Self::CommentReaction { .. }
                | Self::ReviewReaction { .. }
                | Self::RevisionReaction { .. }
        )
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{path::Path, str::FromStr as _};

    use radicle::{
        cob::{EntryId, ObjectId},
        identity::{Did, RepoId},
    };

    use crate::{
        agent::{Context, DeliveryId, Error, McpEndpoint, Talker},
        member::Profile,
        watcher::activity::{Change, Comment, Delivery, InvalidSubject, Subject, Target},
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";
    const OPERATION: &str = "2222222222222222222222222222222222222222";
    const COMMENT: &str = "3333333333333333333333333333333333333333";
    const AUTHOR: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Mentioned(Context, Target),
        Replied(Context, Comment, EntryId),
        Changed(Context, Change),
    }

    #[derive(Debug, Default)]
    struct RecordingAgent {
        calls: Vec<Call>,
    }

    impl crate::agent::Supervised for RecordingAgent {
        fn supervisor(&self) -> std::sync::Arc<dyn crate::agent::Supervisor> {
            std::sync::Arc::new(crate::agent::Idle)
        }
    }

    #[async_trait::async_trait]
    impl Talker for RecordingAgent {
        async fn start(
            &mut self,
            _profile: Profile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
            _layout: crate::agent::Layout,
        ) -> Result<(), Error> {
            Ok(())
        }

        async fn mentioned(&mut self, context: Context, target: Target) -> Result<(), Error> {
            self.calls.push(Call::Mentioned(context, target));
            Ok(())
        }

        async fn replied(
            &mut self,
            context: Context,
            comment: Comment,
            reply_to: EntryId,
        ) -> Result<(), Error> {
            self.calls.push(Call::Replied(context, comment, reply_to));
            Ok(())
        }

        async fn changed(&mut self, context: Context, change: Change) -> Result<(), Error> {
            self.calls.push(Call::Changed(context, change));
            Ok(())
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
            workspace: Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-workspace"),
        }
    }

    #[tokio::test]
    async fn dispatches_every_delivery_to_its_typed_agent_method() {
        let comment =
            Comment::Issue(EntryId::from_str(COMMENT).expect("comment operation should be valid"));
        let mut agent = RecordingAgent::default();
        let deliveries = [
            Delivery::Mentioned(Target::Comment(comment)),
            Delivery::Replied {
                comment,
                reply_to: context().delivery.operation,
            },
            Delivery::Changed(Change::CommentCreated(comment)),
        ];

        for delivery in deliveries {
            delivery
                .dispatch(context(), &mut agent)
                .await
                .expect("delivery should dispatch");
        }

        assert_eq!(
            agent.calls,
            [
                Call::Mentioned(context(), Target::Comment(comment)),
                Call::Replied(context(), comment, context().delivery.operation),
                Call::Changed(context(), Change::CommentCreated(comment)),
            ]
        );
    }

    #[test]
    fn reads_back_every_subject_address_it_writes() {
        let issue = Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should be valid"));
        let patch = Subject::Patch(ObjectId::from_str(SUBJECT).expect("subject should be valid"));

        for subject in [issue, patch] {
            assert_eq!(
                Subject::from_str(&subject.to_string()),
                Ok(subject),
                "a queued job is only traceable if its stored source survives a round trip"
            );
        }
        for malformed in [SUBJECT.to_owned(), format!("review:{SUBJECT}")] {
            assert!(
                matches!(
                    Subject::from_str(&malformed),
                    Err(InvalidSubject(value)) if value == malformed
                ),
                "an address that names no issue or patch must be refused rather than guessed at"
            );
        }
    }
}
