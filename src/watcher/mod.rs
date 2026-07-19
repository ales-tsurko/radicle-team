//! Read-only Radicle activity watcher.
//!
//! A [`Watcher`](crate::watcher::Watcher) observes one repository as one
//! [`Member`](crate::member::Member) and emits [`Activity`](crate::watcher::Activity) values for
//! interactions requiring that member's attention. It reads Radicle state but never changes
//! collaborative objects.
//!
//! # Lifecycle
//!
//! 1. Receive a profile already provisioned and validated by the identity subsystem.
//! 2. Connect to the local node and verify that its identity matches the profile.
//! 3. Replay every issue and patch operation to reconstruct classification state.
//! 4. Emit only operations not recorded in the member's local state database.
//! 5. Subscribe to node events and rescan after relevant collaborative-object references are
//!    fetched.
//!
//! Replaying full histories is intentional: assignments and ownership depend on earlier operations.
//! The SQLite state store suppresses duplicate output while still allowing the in-memory
//! classification context to be rebuilt on every scan. Assignment notifications are delayed until
//! replay completes so closed, merged, or subsequently unassigned objects do not produce stale
//! work.
//!
//! # Classification architecture
//!
//! Radicle issue and patch action enums are isolated behind adapter modules. Each adapter
//! implements `From<ActionEnvelope<_>> for NormalizedAction`, translating its concrete actions into
//! shared assignment updates, ownership updates, and ordered relevance signals. A single generic
//! classification context reduces that normalized form into public
//! [`Activity`](crate::watcher::Activity) values.
//!
//! This boundary keeps Radicle-specific variant matching out of the reducer. Adding an upstream
//! action normally changes only its adapter and reuses the existing normalized behavior; only
//! genuinely new semantics require extending the shared classification model.
//!
//! # Structure
//!
//! - `activity` defines the public activity, subject, target, and change model.
//! - `classification` contains the normalized action model and generic reducer.
//! - `issue` and `patch` adapt Radicle action enums into normalized actions.
//! - `state` persists processed operation IDs for idempotent rescanning.

use std::{path::Path, time::Duration};

use radicle::{
    cob::{
        ObjectId, Op, TypedId,
        issue::{IssueStream, Issues, State as IssueState},
        patch::{PatchStream, Patches, State as PatchState},
        store::access::ReadOnly,
        stream::CobStream as _,
    },
    identity::RepoId,
    node::{Handle as _, Node, NodeId, events::Event as NodeEvent},
    profile::Profile as RadicleProfile,
    storage::{ReadStorage as _, RefUpdate, git::Repository},
};

use crate::{
    member::Member,
    watcher::{
        classification::{ActionEnvelope, Context as ClassificationContext, NormalizedAction},
        state::ProcessedOperations,
    },
};
pub use crate::watcher::activity::{Activity, Change, Comment, Kind, Subject, Target};

mod activity;
mod classification;
mod issue;
mod patch;
mod state;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

macro_rules! scan_store {
    (
        $objects:expr,
        $watcher:expr,
        $repository:expr,
        $member_id:expr,
        $state:expr,
        $emit:expr,
        $stream:ident,
        $subject:path,
        $actionable:expr
    ) => {{
        $objects.try_for_each(|object| {
            let (id, object) = object?;
            let stream = $stream::init(id, $repository);
            let scope = ScanScope::new(
                $watcher.member,
                $member_id,
                $watcher.repository,
                id,
                $subject(id),
                $actionable(&object),
            );
            Watcher::scan_history(stream.all()?, scope, $state, $emit)
        })
    }};
}

macro_rules! store_scanner {
    ($name:ident, $store:ident, $stream:ident, $subject:path, $actionable:expr) => {
        fn $name(
            &self,
            repository: &Repository,
            member_id: NodeId,
            state: &ProcessedOperations,
            emit: &mut impl FnMut(Activity),
        ) -> Result<(), Error> {
            let objects = $store::open(repository, ReadOnly)?;
            scan_store!(
                objects.all()?,
                self,
                repository,
                member_id,
                state,
                emit,
                $stream,
                $subject,
                $actionable
            )
        }
    };
}

/// Watches a repository for activity addressed to one team member.
#[derive(Debug, Clone)]
pub struct Watcher {
    member: Member,
    repository: RepoId,
}

impl Watcher {
    store_scanner!(
        scan_patches,
        Patches,
        PatchStream,
        Subject::Patch,
        |patch: &radicle::cob::patch::Patch| matches!(
            patch.state(),
            PatchState::Open { .. } | PatchState::Draft
        )
    );

    store_scanner!(
        scan_issues,
        Issues,
        IssueStream,
        Subject::Issue,
        |issue: &radicle::cob::issue::Issue| matches!(issue.state(), IssueState::Open)
    );

    /// Creates a watcher for `member` and `repository`.
    #[must_use]
    pub const fn new(member: Member, repository: RepoId) -> Self {
        Self { member, repository }
    }

    /// Watches with an identity profile already validated by the controller.
    pub(crate) fn run(
        &self,
        profile: &RadicleProfile,
        state_path: &Path,
        emit: impl FnMut(Activity),
    ) -> Result<(), Error> {
        let node = Node::new(profile.home().socket_default());
        let member_id = *profile.id();
        Self::verify_identity(member_id, node.nid()?)?;
        self.run_with_node(profile, &node, member_id, state_path, emit)
    }

    fn run_with_node(
        &self,
        profile: &RadicleProfile,
        node: &Node,
        member_id: NodeId,
        state_path: &Path,
        emit: impl FnMut(Activity),
    ) -> Result<(), Error> {
        let repository = profile.storage.repository(self.repository)?;
        let state = ProcessedOperations::open(state_path)?;
        let mut events = node.subscribe(EVENT_TIMEOUT)?;
        self.watch(&repository, member_id, &state, &mut events, emit)
    }

    fn watch(
        &self,
        repository: &Repository,
        member_id: NodeId,
        state: &ProcessedOperations,
        events: &mut impl Iterator<Item = Result<NodeEvent, radicle::node::Error>>,
        mut emit: impl FnMut(Activity),
    ) -> Result<(), Error> {
        self.scan_all(repository, member_id, state, &mut emit)?;
        loop {
            self.rescan_after_event(repository, member_id, state, events, &mut emit)?;
        }
    }

    fn rescan_after_event(
        &self,
        repository: &Repository,
        member_id: NodeId,
        state: &ProcessedOperations,
        events: &mut impl Iterator<Item = Result<NodeEvent, radicle::node::Error>>,
        emit: &mut impl FnMut(Activity),
    ) -> Result<(), Error> {
        if self.next_event_updates_cobs(events)? {
            self.scan_all(repository, member_id, state, emit)?;
        }
        Ok(())
    }

    fn scan_all(
        &self,
        repository: &Repository,
        member_id: NodeId,
        state: &ProcessedOperations,
        emit: &mut impl FnMut(Activity),
    ) -> Result<(), Error> {
        self.scan_issues(repository, member_id, state, emit)?;
        self.scan_patches(repository, member_id, state, emit)
    }

    fn scan_history<A>(
        operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
        scope: ScanScope,
        state: &ProcessedOperations,
        emit: &mut impl FnMut(Activity),
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut context = ClassificationContext::default();
        let mut pending = PendingActivities::default();
        for operation in operations {
            let operation = operation?;
            let new = state.insert(scope.repository, scope.object, operation.id)?;
            let activities = context.classify(
                &operation,
                scope.member,
                scope.member_id,
                scope.repository,
                scope.subject,
            );
            if new {
                activities
                    .into_iter()
                    .for_each(|activity| pending.add(activity));
            }
        }
        pending.emit(context.assigned(), scope.actionable, emit);
        Ok(())
    }

    fn next_event_updates_cobs(
        &self,
        events: &mut impl Iterator<Item = Result<NodeEvent, radicle::node::Error>>,
    ) -> Result<bool, Error> {
        let event = events.next().ok_or(Error::EventStreamClosed)?;
        self.event_updates_cobs(event)
    }

    fn event_updates_cobs(
        &self,
        event: Result<NodeEvent, radicle::node::Error>,
    ) -> Result<bool, Error> {
        match event {
            Ok(NodeEvent::RefsFetched { rid, updated, .. }) => {
                Ok(rid == self.repository && Self::updates_cobs(&updated))
            }
            Ok(_) | Err(radicle::node::Error::TimedOut) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn verify_identity(profile: NodeId, node: NodeId) -> Result<(), Error> {
        if profile == node {
            Ok(())
        } else {
            Err(Error::IdentityMismatch { profile, node })
        }
    }

    fn updates_cobs(updates: &[RefUpdate]) -> bool {
        updates.iter().any(|update| {
            update
                .name()
                .to_namespaced()
                .and_then(|name| TypedId::from_namespaced(&name).ok().flatten())
                .is_some_and(|id| id.is_issue() || id.is_patch())
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ScanScope {
    member: Member,
    member_id: NodeId,
    repository: RepoId,
    object: ObjectId,
    subject: Subject,
    actionable: bool,
}

impl ScanScope {
    const fn new(
        member: Member,
        member_id: NodeId,
        repository: RepoId,
        object: ObjectId,
        subject: Subject,
        actionable: bool,
    ) -> Self {
        Self {
            member,
            member_id,
            repository,
            object,
            subject,
            actionable,
        }
    }
}

#[derive(Default)]
struct PendingActivities {
    assignment: Option<Activity>,
    activities: Vec<Activity>,
}

impl PendingActivities {
    fn add(&mut self, activity: Activity) {
        match activity.kind {
            Kind::Assigned => self.assignment = Some(activity),
            _ => self.activities.push(activity),
        }
    }

    fn emit(self, assigned: bool, actionable: bool, emit: &mut impl FnMut(Activity)) {
        if assigned
            && actionable
            && let Some(activity) = self.assignment
        {
            emit(activity);
        }
        self.activities.into_iter().for_each(emit);
    }
}

/// Error produced by the Radicle watcher.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Communicating with the Radicle node failed.
    #[error("Radicle node error: {0}")]
    Node(#[from] radicle::node::Error),
    /// The node and profile use different identities.
    #[error("Radicle node identity {node} does not match profile identity {profile}")]
    IdentityMismatch {
        /// Identity loaded from the profile.
        profile: NodeId,
        /// Identity reported by the node.
        node: NodeId,
    },
    /// Opening the Radicle repository failed.
    #[error("failed to open Radicle repository: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    /// Reading a collaborative object store failed.
    #[error("failed to read Radicle collaborative objects: {0}")]
    CobStore(#[from] radicle::cob::store::Error),
    /// Opening a collaborative object operation stream failed.
    #[error("failed to open collaborative object operation stream: {0}")]
    CobStream(#[from] radicle::cob::stream::error::Stream),
    /// Reading a collaborative object operation failed.
    #[error("failed to read collaborative object operation: {0}")]
    CobOperation(#[from] radicle::cob::stream::error::Ops),
    /// Reading or writing the local watcher database failed.
    #[error("watcher database error: {0}")]
    Database(#[from] sqlite::Error),
    /// Preparing the local watcher database failed.
    #[error("watcher state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The node closed its event stream.
    #[error("Radicle node closed its event stream")]
    EventStreamClosed,
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, str::FromStr as _};

    use radicle::{
        cob::{
            EntryId, Manifest, ObjectId, Op, Reaction, Timestamp, Title, TypeName, Version,
            issue::{Action, State as IssueState, TYPENAME},
            patch::{
                Action as PatchAction, Lifecycle, MergeTarget, ReviewEdit, ReviewId, RevisionId,
                TYPENAME as PATCH_TYPENAME, Verdict,
            },
        },
        crypto::PublicKey,
        identity::{Did, RepoId},
    };

    use crate::{
        member::Member,
        watcher::{
            Activity, Change, Comment, Kind, Subject,
            classification::{ActionEnvelope, Context, NormalizedAction},
        },
    };

    const MEMBER: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
    const AUTHOR: &str = "z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";
    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const ISSUE: &str = "1111111111111111111111111111111111111111";
    const OPERATION: &str = "2222222222222222222222222222222222222222";
    const COMMENT: &str = "3333333333333333333333333333333333333333";
    const CHANGE: &str = "4444444444444444444444444444444444444444";
    const REPLY: &str = "5555555555555555555555555555555555555555";

    struct Fixture;

    impl Fixture {
        fn operation<A>(id: &str, action: A, author: PublicKey, type_name: TypeName) -> Op<A> {
            Op::new(
                EntryId::from_str(id).expect("operation should be valid"),
                (action, Vec::new()),
                author,
                Timestamp::from_secs(0),
                None,
                Manifest::new(type_name, Version::default()),
            )
        }

        fn classify<A>(
            id: &str,
            action: A,
            author: PublicKey,
            type_name: TypeName,
            subject: Subject,
            context: &mut Context,
        ) -> Vec<Activity>
        where
            for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
        {
            context.classify(
                &Self::operation(id, action, author, type_name),
                Member::Nico,
                PublicKey::from_str(MEMBER).expect("member key should be valid"),
                RepoId::from_str(REPOSITORY).expect("repository should be valid"),
                subject,
            )
        }

        fn classify_issue(
            action: Action,
            author: PublicKey,
            context: &mut Context,
        ) -> Vec<Activity> {
            Self::classify(
                OPERATION,
                action,
                author,
                TYPENAME.clone(),
                Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid")),
                context,
            )
        }

        fn classify_patch(
            id: &str,
            action: PatchAction,
            author: PublicKey,
            context: &mut Context,
        ) -> Vec<Activity> {
            Self::classify(
                id,
                action,
                author,
                PATCH_TYPENAME.clone(),
                Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid")),
                context,
            )
        }
    }

    #[test]
    fn detects_member_mentions() {
        assert!(Member::Nico.is_mentioned_in("Could you check this, @Nico?"));
        assert!(Member::Nico.is_mentioned_in("(@nico) please investigate"));
    }

    #[test]
    fn rejects_partial_or_embedded_mentions() {
        assert!(!Member::Nico.is_mentioned_in("Ask @nicole"));
        assert!(!Member::Nico.is_mentioned_in("mail@example@nico"));
        assert!(!Member::Nico.is_mentioned_in("nico"));
    }

    #[test]
    fn classifies_assignment_changes() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let mut context = Context::default();
        let assignment = Action::Assign {
            assignees: BTreeSet::from([Did::from(member)]),
        };

        assert!(matches!(
            Fixture::classify_issue(assignment.clone(), author, &mut context).as_slice(),
            [Activity {
                kind: Kind::Assigned,
                ..
            }]
        ));
        assert!(context.assigned());
        assert!(Fixture::classify_issue(assignment, author, &mut context).is_empty());
        assert!(matches!(
            Fixture::classify_issue(
                Action::Assign {
                    assignees: BTreeSet::new()
                },
                author,
                &mut context
            )
            .as_slice(),
            [Activity {
                kind: Kind::Changed(Change::Unassigned),
                ..
            }]
        ));
        assert!(!context.assigned());
    }

    #[test]
    fn classifies_external_mentions_only() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let mut context = Context::default();
        let comment = Action::Comment {
            body: "Hey @nico".to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        };

        assert!(matches!(
            Fixture::classify_issue(comment.clone(), author, &mut context).as_slice(),
            [Activity {
                kind: Kind::Mentioned(_),
                ..
            }]
        ));
        assert!(Fixture::classify_issue(comment, member, &mut context).is_empty());
        assert!(matches!(
            Fixture::classify_issue(
                Action::CommentEdit {
                    id: EntryId::from_str(ISSUE).expect("comment should be valid"),
                    body: "@Nico, updated".to_owned(),
                    embeds: Vec::new(),
                },
                author,
                &mut context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Mentioned(_),
                ..
            }]
        ));
    }

    #[test]
    fn classifies_issue_replies_and_owned_discussion() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let mut context = Context::default();
        let member_comment = Action::Comment {
            body: "I will investigate".to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        };

        assert!(Fixture::classify_issue(member_comment, member, &mut context).is_empty());
        assert!(matches!(
            Fixture::classify(
                COMMENT,
                Action::Comment {
                    body: "Here are the details".to_owned(),
                    reply_to: Some(EntryId::from_str(OPERATION).expect("comment should be valid")),
                    embeds: Vec::new(),
                },
                author,
                TYPENAME.clone(),
                Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid")),
                &mut context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Replied { .. },
                ..
            }]
        ));
    }

    #[test]
    fn classifies_patch_assignment_and_review() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let mut assignment_context = Context::default();

        assert!(matches!(
            Fixture::classify_patch(
                OPERATION,
                PatchAction::Assign {
                    assignees: BTreeSet::from([Did::from(member)])
                },
                author,
                &mut assignment_context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Assigned,
                ..
            }]
        ));

        let mut owned_context = Context::default();
        assert!(
            Fixture::classify_patch(
                OPERATION,
                PatchAction::Revision {
                    description: "Initial revision".to_owned(),
                    base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                    oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                    resolves: BTreeSet::new(),
                },
                member,
                &mut owned_context,
            )
            .is_empty()
        );
        assert!(matches!(
            Fixture::classify_patch(
                COMMENT,
                PatchAction::Review {
                    revision: RevisionId::from(
                        EntryId::from_str(OPERATION).expect("revision should be valid")
                    ),
                    summary: Some("Looks good".to_owned()),
                    verdict: Some(Verdict::Accept),
                    labels: Vec::new(),
                },
                author,
                &mut owned_context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Changed(Change::ReviewSubmitted { .. }),
                ..
            }]
        ));
    }

    #[test]
    fn classifies_patch_mentions_and_replies() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let revision =
            RevisionId::from(EntryId::from_str(ISSUE).expect("revision should be valid"));
        let review = ReviewId::from(EntryId::from_str(ISSUE).expect("review should be valid"));
        let mut context = Context::default();

        assert!(
            Fixture::classify_patch(
                OPERATION,
                PatchAction::RevisionComment {
                    revision,
                    location: None,
                    body: "My comment".to_owned(),
                    reply_to: None,
                    embeds: Vec::new(),
                },
                member,
                &mut context,
            )
            .is_empty()
        );
        assert!(matches!(
            Fixture::classify_patch(
                COMMENT,
                PatchAction::RevisionComment {
                    revision,
                    location: None,
                    body: "A reply".to_owned(),
                    reply_to: Some(EntryId::from_str(OPERATION).expect("comment should be valid")),
                    embeds: Vec::new(),
                },
                author,
                &mut context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Replied {
                    comment: Comment::Revision { .. },
                    ..
                },
                ..
            }]
        ));
        assert!(matches!(
            Fixture::classify_patch(
                COMMENT,
                PatchAction::ReviewComment {
                    review,
                    body: "Please check this, @nico".to_owned(),
                    location: None,
                    reply_to: None,
                    embeds: Vec::new(),
                },
                author,
                &mut context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Mentioned(_),
                ..
            }]
        ));
    }

    #[test]
    fn classifies_every_remaining_issue_interaction() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let mut context = Context::default();
        assert!(
            Fixture::classify_issue(
                Action::Comment {
                    body: "Owned discussion".to_owned(),
                    reply_to: None,
                    embeds: Vec::new(),
                },
                member,
                &mut context,
            )
            .is_empty()
        );

        let comment = EntryId::from_str(OPERATION).expect("comment should be valid");
        let reaction = Reaction::new('👍').expect("reaction should be valid");
        let actions = [
            Action::Edit {
                title: Title::new("Updated issue").expect("title should be valid"),
            },
            Action::Lifecycle {
                state: IssueState::Open,
            },
            Action::Label {
                labels: BTreeSet::new(),
            },
            Action::Comment {
                body: "Additional context".to_owned(),
                reply_to: None,
                embeds: Vec::new(),
            },
            Action::CommentEdit {
                id: comment,
                body: "Edited context".to_owned(),
                embeds: Vec::new(),
            },
            Action::CommentRedact { id: comment },
            Action::CommentReact {
                id: comment,
                reaction,
                active: true,
            },
        ];

        for action in actions {
            assert!(!Fixture::classify_issue(action, author, &mut context).is_empty());
        }
    }

    #[test]
    fn classifies_every_remaining_patch_interaction() {
        let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
        let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
        let revision =
            RevisionId::from(EntryId::from_str(OPERATION).expect("revision should be valid"));
        let review = ReviewId::from(EntryId::from_str(COMMENT).expect("review should be valid"));
        let review_comment = EntryId::from_str(CHANGE).expect("comment should be valid");
        let revision_comment = EntryId::from_str(REPLY).expect("comment should be valid");
        let reaction = Reaction::new('👍').expect("reaction should be valid");
        let mut context = Context::default();

        let member_actions = [
            (
                OPERATION,
                PatchAction::Revision {
                    description: "Initial revision".to_owned(),
                    base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                    oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                    resolves: BTreeSet::new(),
                },
            ),
            (
                COMMENT,
                PatchAction::Review {
                    revision,
                    summary: Some("My review".to_owned()),
                    verdict: Some(Verdict::Accept),
                    labels: Vec::new(),
                },
            ),
            (
                CHANGE,
                PatchAction::ReviewComment {
                    review,
                    body: "Review comment".to_owned(),
                    location: None,
                    reply_to: None,
                    embeds: Vec::new(),
                },
            ),
            (
                REPLY,
                PatchAction::RevisionComment {
                    revision,
                    location: None,
                    body: "Revision comment".to_owned(),
                    reply_to: None,
                    embeds: Vec::new(),
                },
            ),
        ];
        for (id, action) in member_actions {
            assert!(Fixture::classify_patch(id, action, member, &mut context).is_empty());
        }

        let actions = [
            PatchAction::Edit {
                title: Title::new("Updated patch").expect("title should be valid"),
                target: MergeTarget::Delegates,
            },
            PatchAction::Label {
                labels: BTreeSet::new(),
            },
            PatchAction::Lifecycle {
                state: Lifecycle::Archived,
            },
            PatchAction::Merge {
                revision,
                commit: *ObjectId::from_str(COMMENT).expect("commit should be valid"),
            },
            PatchAction::ReviewEdit(ReviewEdit::new(
                review,
                "Updated review".to_owned(),
                Some(Verdict::Reject),
                Vec::new(),
                Vec::new(),
            )),
            PatchAction::ReviewRedact { review },
            PatchAction::ReviewReact {
                review,
                reaction,
                active: true,
            },
            PatchAction::ReviewCommentEdit {
                review,
                comment: review_comment,
                body: "Edited review comment".to_owned(),
                embeds: Vec::new(),
            },
            PatchAction::ReviewCommentRedact {
                review,
                comment: review_comment,
            },
            PatchAction::ReviewCommentReact {
                review,
                comment: review_comment,
                reaction,
                active: true,
            },
            PatchAction::ReviewCommentResolve {
                review,
                comment: review_comment,
            },
            PatchAction::ReviewCommentUnresolve {
                review,
                comment: review_comment,
            },
            PatchAction::RevisionEdit {
                revision,
                description: "Edited revision".to_owned(),
                embeds: Vec::new(),
            },
            PatchAction::RevisionRedact { revision },
            PatchAction::RevisionReact {
                revision,
                location: None,
                reaction,
                active: true,
            },
            PatchAction::RevisionCommentEdit {
                revision,
                comment: revision_comment,
                body: "Edited revision comment".to_owned(),
                embeds: Vec::new(),
            },
            PatchAction::RevisionCommentRedact {
                revision,
                comment: revision_comment,
            },
            PatchAction::RevisionCommentReact {
                revision,
                comment: revision_comment,
                reaction,
                active: true,
            },
        ];

        for action in actions {
            assert!(!Fixture::classify_patch(ISSUE, action, author, &mut context).is_empty());
        }
    }
}
