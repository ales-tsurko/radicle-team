//! Activities which may require the selected member's attention.

use radicle::{
    cob::{
        EntryId, ObjectId,
        patch::{ReviewId, RevisionId},
    },
    identity::{Did, RepoId},
};

/// Collaborative object receiving an interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// Radicle issue.
    Issue(ObjectId),
    /// Radicle patch.
    Patch(ObjectId),
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

/// Reason an interaction requires the member's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
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

/// Activity addressed to or otherwise relevant to the selected member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Repository containing the activity.
    pub repository: RepoId,
    /// Issue or patch receiving the activity.
    pub subject: Subject,
    /// Operation which produced the activity.
    pub operation: EntryId,
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
        author: Did,
        kind: Kind,
    ) -> Self {
        Self {
            repository,
            subject,
            operation,
            author,
            kind,
        }
    }
}
