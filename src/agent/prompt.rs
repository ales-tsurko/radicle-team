//! What a role is told about the delivery it has been handed.
//!
//! Facts only. Who the member is, how it writes, what its role does, and which tools it has are in
//! the documents instructing it and in the tools themselves, all of which say it better than a
//! prompt assembled per delivery could. What belongs here is what neither can know: what just
//! happened, where, and who did it.

use radicle::cob::EntryId;

use crate::{
    agent::interface::{ChatContext, ChatMessage, Context},
    queue::Task,
    watcher::{Change, Comment, Target},
};

/// Describes a mention in an issue or patch.
pub(crate) fn mentioned(context: &Context, target: Target) -> String {
    format!(
        "You were mentioned in {}, {}. It was written by {}.",
        context.subject,
        located(target),
        context.author
    )
}

/// Describes a reply to one of the member's own comments.
pub(crate) fn replied(context: &Context, comment: Comment, reply_to: EntryId) -> String {
    format!(
        "{} replied to your comment {reply_to} in {}: the reply is {}.",
        context.author,
        context.subject,
        commented(comment)
    )
}

/// Describes a change to something the member owns, reviews, or is assigned to.
pub(crate) fn changed(context: &Context, change: Change) -> String {
    format!(
        "{} changed {}: {}.",
        context.author,
        context.subject,
        described(change)
    )
}

/// Describes an authenticated chat message addressed to the member.
///
/// The sender's nick is theirs to choose, so it never opens the message: this is handed to a
/// command line, where a first word beginning with a dash is an option rather than text.
pub(crate) fn messaged(context: &ChatContext, message: &ChatMessage) -> String {
    format!(
        "In the {} channel, {} ({}) addressed you:\n\n{}",
        context.channel(),
        message.nick,
        context.author(),
        message.body
    )
}

/// Describes one queued task.
pub(crate) fn task(task: &Task) -> String {
    if task.context.cancelled {
        return stopping(task);
    }
    format!(
        "Task {} came from {}, and its agreed specification is:\n\n{}\n\nRelated artifacts: \
         {:?}\nPinned messages: {:?}\nRetained notes: {:?}\n\nReread pinned messages and current \
         artifact state before acting on earlier feedback. Save important findings with \
         update_task_context. Use the existing patch for revisions.",
        task.id,
        task.source,
        task.context.specification,
        task.context.artifacts,
        task.context.messages,
        task.context.notes
    )
}

/// Asks a stopped task to preserve what it reached.
///
/// Its own run, because an agent turn cannot be interrupted: the run carrying out the task is ended
/// first, and this one is what commits whatever that left in the working copy. A fresh run has no
/// memory of the work, which is why it is told to look at the tree rather than to recall it.
pub(crate) fn stopping(task: &Task) -> String {
    format!(
        "You were asked to stop task {} before it was finished, and this run is the one that \
         preserves it: the run doing the work has already been ended, so read the tree to find \
         out where it got to. Commit what is uncommitted to the branch this copy is standing on, \
         with a message saying what remains. Start nothing new.",
        task.id
    )
}

// Returns where in an issue or patch a mention was written.
fn located(target: Target) -> String {
    match target {
        Target::Subject => "in its title or description".to_owned(),
        Target::Comment(comment) => format!("in {}", commented(comment)),
        Target::Review(review) => format!("in the summary of review {review}"),
        Target::Revision(revision) => format!("in the description of revision {revision}"),
    }
}

// Returns where in an issue or patch a comment sits.
fn commented(comment: Comment) -> String {
    match comment {
        Comment::Issue(comment) => format!("comment {comment}"),
        Comment::Revision { revision, comment } => {
            format!("comment {comment} on revision {revision}")
        }
        Comment::Review { review, comment } => format!("comment {comment} in review {review}"),
    }
}

// Returns what happened, in the words somebody watching the repository would use.
fn described(change: Change) -> String {
    match change {
        Change::Unassigned => "you were taken off it".to_owned(),
        Change::Edited => "its title, target, or other metadata was edited".to_owned(),
        Change::LabelsChanged => "its labels were replaced".to_owned(),
        Change::LifecycleChanged => "it was opened, closed, or reopened".to_owned(),
        Change::PatchReady => "the patch is ready for review".to_owned(),
        Change::Merged => "the patch was merged".to_owned(),
        Change::CommentCreated(comment) => format!("{} was added", commented(comment)),
        Change::CommentEdited(comment) => format!("{} was edited", commented(comment)),
        Change::CommentRedacted(comment) => format!("{} was redacted", commented(comment)),
        Change::CommentReaction { comment, active } => {
            format!("a reaction on {} was {}", commented(comment), added(active))
        }
        Change::CommentResolution { comment, resolved } => format!(
            "{} was {}",
            commented(comment),
            match resolved {
                true => "resolved",
                false => "reopened",
            }
        ),
        Change::ReviewCommentsResolved => {
            "a new revision resolved review comments of yours".to_owned()
        }
        Change::ReviewSubmitted { review, revision } => {
            format!("review {review} was submitted on revision {revision}")
        }
        Change::ReviewEdited(review) => format!("review {review} was edited"),
        Change::ReviewRedacted(review) => format!("review {review} was redacted"),
        Change::ReviewReaction { review, active } => {
            format!("a reaction on review {review} was {}", added(active))
        }
        Change::RevisionCreated(revision) => format!("revision {revision} was created"),
        Change::RevisionEdited(revision) => format!("revision {revision} was edited"),
        Change::RevisionRedacted(revision) => format!("revision {revision} was redacted"),
        Change::RevisionReaction { revision, active } => {
            format!("a reaction on revision {revision} was {}", added(active))
        }
    }
}

const fn added(active: bool) -> &'static str {
    match active {
        true => "added",
        false => "removed",
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr as _};

    use radicle::{
        cob::{EntryId, ObjectId},
        identity::{Did, RepoId},
    };

    use crate::{
        agent::{
            interface::{Context, DeliveryId},
            prompt,
        },
        manifest::QueuePosition,
        queue::Task,
        watcher::{Change, Comment, Subject, Target},
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const OBJECT: &str = "1111111111111111111111111111111111111111";
    const AUTHOR: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[test]
    fn tells_the_talking_role_what_happened_and_where() {
        let context = context();
        let mentioned = prompt::mentioned(&context, Target::Comment(Comment::Issue(entry())));

        assert!(mentioned.contains(&context.subject.to_string()));
        assert!(mentioned.contains(AUTHOR));
        assert!(
            mentioned.contains(&entry().to_string()),
            "a mention has to say which comment it is in, or the agent has to read the whole \
             thread to find itself"
        );

        let replied = prompt::replied(&context, Comment::Issue(entry()), entry());
        assert!(replied.contains(&entry().to_string()));

        for change in [
            Change::Unassigned,
            Change::Edited,
            Change::LabelsChanged,
            Change::LifecycleChanged,
            Change::PatchReady,
            Change::Merged,
            Change::CommentCreated(Comment::Issue(entry())),
            Change::CommentEdited(Comment::Issue(entry())),
            Change::CommentRedacted(Comment::Issue(entry())),
            Change::CommentReaction {
                comment: Comment::Issue(entry()),
                active: true,
            },
            Change::CommentResolution {
                comment: Comment::Issue(entry()),
                resolved: false,
            },
            Change::ReviewCommentsResolved,
            Change::ReviewSubmitted {
                review: entry().into(),
                revision: entry().into(),
            },
            Change::ReviewEdited(entry().into()),
            Change::ReviewRedacted(entry().into()),
            Change::ReviewReaction {
                review: entry().into(),
                active: false,
            },
            Change::RevisionCreated(entry().into()),
            Change::RevisionEdited(entry().into()),
            Change::RevisionRedacted(entry().into()),
            Change::RevisionReaction {
                revision: entry().into(),
                active: true,
            },
        ] {
            let described = prompt::changed(&context, change);
            assert!(
                described.contains(&context.subject.to_string()) && described.contains(AUTHOR),
                "{change:?} must say what was changed and by whom"
            );
        }
    }

    #[test]
    fn hands_the_building_role_its_brief_and_asks_a_stopped_one_to_commit() {
        let task = Task {
            id: 17,
            brief: "replace the parser".to_owned(),
            context: crate::queue::TaskContext {
                specification: "replace the parser".to_owned(),
                ..crate::queue::TaskContext::default()
            },
            source: Subject::Issue(object()).into(),
            position: QueuePosition::End,
        };

        let carrying = prompt::task(&task);
        assert!(carrying.contains("replace the parser"));
        assert!(carrying.contains("17"));
        assert!(carrying.contains(&task.source.to_string()));

        let stopping = prompt::stopping(&task);
        assert!(stopping.contains("Commit"), "{stopping}");
        assert!(
            stopping.contains("Start nothing new"),
            "a stopped task which starts something else has not stopped"
        );
    }

    #[test]
    fn tells_the_chatting_role_who_is_waiting_for_it() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/prompt-chat");
        let context = crate::agent::interface::ChatContext::authenticated(
            repository(),
            "general".parse().expect("channel should be valid"),
            crate::chat::MessageId::new(3).expect("message should be valid"),
            crate::chat::AuthenticatedSender::for_test(author()),
            workspace,
        );
        let message = crate::agent::interface::ChatMessage {
            nick: "alex".to_owned(),
            body: "@nico can you look at the parser?".to_owned(),
        };

        let addressed = prompt::messaged(&context, &message);

        assert!(addressed.contains("alex"));
        assert!(addressed.contains("general"));
        assert!(addressed.contains("can you look at the parser?"));
        assert!(
            addressed.contains(AUTHOR),
            "chat is answered on the strength of who the account was attested as, so the agent \
             has to be told which identity that is"
        );
    }

    fn context() -> Context {
        Context {
            repository: repository(),
            subject: Subject::Issue(object()),
            delivery: DeliveryId {
                operation: entry(),
                action: 0,
            },
            author: author(),
            workspace: PathBuf::from("/member/code/talker"),
        }
    }

    fn repository() -> RepoId {
        RepoId::from_urn(REPOSITORY).expect("repository should parse")
    }

    fn object() -> ObjectId {
        ObjectId::from_str(OBJECT).expect("object should parse")
    }

    fn entry() -> EntryId {
        EntryId::from_str(OBJECT).expect("entry should parse")
    }

    fn author() -> Did {
        Did::from_str(AUTHOR).expect("author should parse")
    }
}
