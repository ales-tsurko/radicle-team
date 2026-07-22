//! Watcher lifecycle, scanning, and event-stream orchestration.

use std::{path::Path, time::Duration};

use radicle::{
    cob::{
        ObjectId, Op, TypedId,
        issue::{Action as IssueAction, IssueStream, Issues, State as IssueState},
        patch::{Action as PatchAction, PatchStream, Patches, State as PatchState},
        store::access::ReadOnly,
        stream::CobStream as _,
    },
    identity::RepoId,
    node::{Handle as _, Node, NodeId, events::Event as NodeEvent},
    profile::Profile as RadicleProfile,
    storage::{ReadStorage as _, RefUpdate, git::Storage},
};

use crate::{
    agent::{self, Agent, Outcome},
    member::Member,
    watcher::{
        activity::{Activity, Kind, Subject},
        classification::{ActionEnvelope, Context as ClassificationContext, NormalizedAction},
        state::ProcessedActivities,
    },
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const HISTORY_BUFFER: usize = 32;

macro_rules! history_scanner {
    ($name:ident, $action:ty, $store:ident, $stream:ident, $actionable:expr) => {
        fn $name(&self, storage: Storage) -> tokio::sync::mpsc::Receiver<HistoryEvent<$action>> {
            self.stream_histories(storage, |repository, sender| {
                let objects = $store::open(&repository, ReadOnly)?;
                for object in objects.all()? {
                    let (id, object) = object?;
                    send_history(
                        sender,
                        HistoryEvent::Begin {
                            object: id,
                            actionable: $actionable(&object),
                        },
                    )?;
                    for operation in $stream::init(id, &repository).all()? {
                        send_history(sender, HistoryEvent::Operation(operation?))?;
                    }
                    send_history(sender, HistoryEvent::End)?;
                }
                Ok(())
            })
        }
    };
}

enum HistoryEvent<A> {
    Begin { object: ObjectId, actionable: bool },
    Operation(Op<A>),
    End,
    Failed(Error),
}

struct ActiveHistory {
    scope: ScanScope,
    context: ClassificationContext,
    pending: PendingActivities,
}

fn send_history<A>(
    sender: &tokio::sync::mpsc::Sender<HistoryEvent<A>>,
    event: HistoryEvent<A>,
) -> Result<(), Error> {
    sender
        .blocking_send(event)
        .map_err(|_send| Error::HistoryStreamClosed)
}

/// Watches a repository for activity addressed to one team member.
#[derive(Debug, Clone)]
pub struct Watcher {
    member: Member,
    repository: RepoId,
}

impl Watcher {
    history_scanner!(
        issue_histories,
        IssueAction,
        Issues,
        IssueStream,
        |issue: &radicle::cob::issue::Issue| matches!(issue.state(), IssueState::Open)
    );

    history_scanner!(
        patch_histories,
        PatchAction,
        Patches,
        PatchStream,
        |patch: &radicle::cob::patch::Patch| matches!(
            patch.state(),
            PatchState::Open { .. } | PatchState::Draft
        )
    );

    /// Creates a watcher for `member` and `repository`.
    #[must_use]
    pub const fn new(member: Member, repository: RepoId) -> Self {
        Self { member, repository }
    }

    /// Watches with an identity profile already validated by the controller.
    pub(crate) async fn run(
        &self,
        profile: &RadicleProfile,
        node: &Node,
        state_path: &Path,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        let member_id = Self::verify_profile(*profile.id(), node.clone()).await?;
        let storage = profile.storage.clone();
        let state = ProcessedActivities::open(state_path).await?;
        let events = self.subscribe(node.clone()).await?;
        self.watch(&storage, member_id, &state, events, agent).await
    }

    async fn verify_profile(member_id: NodeId, node: Node) -> Result<NodeId, Error> {
        let node_id = tokio::task::spawn_blocking(move || node.nid())
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Self::verify_identity(member_id, node_id)?;

        Ok(member_id)
    }

    async fn watch(
        &self,
        storage: &Storage,
        member_id: NodeId,
        state: &ProcessedActivities,
        mut events: tokio::sync::mpsc::Receiver<Result<(), Error>>,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        self.scan_all(storage, member_id, state, agent).await?;
        loop {
            Self::await_update(&mut events).await?;
            self.scan_all(storage, member_id, state, agent).await?;
        }
    }

    async fn await_update(
        events: &mut tokio::sync::mpsc::Receiver<Result<(), Error>>,
    ) -> Result<(), Error> {
        events.recv().await.ok_or(Error::EventStreamClosed)??;
        while let Ok(event) = events.try_recv() {
            event?;
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        node: Node,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<(), Error>>, Error> {
        let watcher = self.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (ready, readiness) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let mut events = match node.subscribe(EVENT_TIMEOUT).map_err(Error::from) {
                Ok(events) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    events
                }
                Err(error) => {
                    let _ignored = ready.send(Err(error));
                    return;
                }
            };
            let result: Result<(), Error> = (|| {
                loop {
                    if watcher.next_event_updates_cobs(&mut events)? {
                        sender
                            .blocking_send(Ok(()))
                            .map_err(|_send| Error::EventStreamClosed)?;
                    }
                }
            })();
            if let Err(error) = result {
                let _ignored = sender.blocking_send(Err(error));
            }
        });
        readiness
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Ok(receiver)
    }

    async fn scan_all(
        &self,
        storage: &Storage,
        member_id: NodeId,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        let issues = self.issue_histories(storage.clone());
        self.scan_objects(issues, member_id, state, agent, Subject::Issue)
            .await?;
        let patches = self.patch_histories(storage.clone());
        self.scan_objects(patches, member_id, state, agent, Subject::Patch)
            .await
    }

    async fn scan_objects<A>(
        &self,
        mut histories: tokio::sync::mpsc::Receiver<HistoryEvent<A>>,
        member_id: NodeId,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        subject: fn(ObjectId) -> Subject,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut active = None;
        while let Some(event) = histories.recv().await {
            self.handle_history_event(event, member_id, subject, &mut active, state, agent)
                .await?;
        }
        Self::complete_history_stream(active)
    }

    fn stream_histories<A, F>(
        &self,
        storage: Storage,
        scan: F,
    ) -> tokio::sync::mpsc::Receiver<HistoryEvent<A>>
    where
        A: Send + 'static,
        F: FnOnce(
                radicle::storage::git::Repository,
                &tokio::sync::mpsc::Sender<HistoryEvent<A>>,
            ) -> Result<(), Error>
            + Send
            + 'static,
    {
        let repository_id = self.repository;
        let (sender, receiver) = tokio::sync::mpsc::channel(HISTORY_BUFFER);
        let failure = sender.clone();
        let task = tokio::task::spawn_blocking(move || {
            let result = storage
                .repository(repository_id)
                .map_err(Error::from)
                .and_then(|repository| scan(repository, &sender));
            if let Err(error) = result {
                let _failed = sender.blocking_send(HistoryEvent::Failed(error));
            }
        });
        tokio::spawn(async move {
            if let Err(error) = task.await {
                let _failed = failure
                    .send(HistoryEvent::Failed(Error::Task(error.to_string())))
                    .await;
            }
        });
        receiver
    }

    async fn handle_history_event<A>(
        &self,
        event: HistoryEvent<A>,
        member_id: NodeId,
        subject: fn(ObjectId) -> Subject,
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        match event {
            HistoryEvent::Begin { object, actionable } => {
                self.begin_history(object, actionable, member_id, subject, active)
            }
            HistoryEvent::Operation(operation) => Self::scan_active(operation, active, state).await,
            HistoryEvent::End => Self::finish_active(active, state, agent).await,
            HistoryEvent::Failed(error) => Err(error),
        }
    }

    fn begin_history(
        &self,
        object: ObjectId,
        actionable: bool,
        member_id: NodeId,
        subject: fn(ObjectId) -> Subject,
        active: &mut Option<ActiveHistory>,
    ) -> Result<(), Error> {
        let history = ActiveHistory {
            scope: ScanScope::new(
                self.member,
                member_id,
                self.repository,
                object,
                subject(object),
                actionable,
            ),
            context: ClassificationContext::default(),
            pending: PendingActivities::default(),
        };
        if active.replace(history).is_some() {
            Err(Error::HistoryProtocol(
                "history began before the previous history ended",
            ))
        } else {
            Ok(())
        }
    }

    async fn scan_active<A>(
        operation: Op<A>,
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let active = active.as_mut().ok_or(Error::HistoryProtocol(
            "operation arrived before history began",
        ))?;
        Self::scan_operation(
            operation,
            active.scope,
            &mut active.context,
            &mut active.pending,
            state,
        )
        .await
    }

    async fn finish_active(
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        let active = active
            .take()
            .ok_or(Error::HistoryProtocol("history ended before it began"))?;
        active
            .pending
            .dispatch(
                active.context.assigned(),
                active.scope.actionable,
                active.scope,
                state,
                agent,
            )
            .await
    }

    fn complete_history_stream(active: Option<ActiveHistory>) -> Result<(), Error> {
        if active.is_some() {
            Err(Error::HistoryProtocol(
                "history stream closed before history ended",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(all(test, not(miri)))]
    async fn scan_history<A>(
        operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut context = ClassificationContext::default();
        let mut pending = PendingActivities::default();
        for operation in operations {
            Self::scan_operation(operation?, scope, &mut context, &mut pending, state).await?;
        }
        pending
            .dispatch(context.assigned(), scope.actionable, scope, state, agent)
            .await
    }

    async fn scan_operation<A>(
        operation: Op<A>,
        scope: ScanScope,
        context: &mut ClassificationContext,
        pending: &mut PendingActivities,
        state: &ProcessedActivities,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let activities = context.classify(
            &operation,
            scope.member,
            scope.member_id,
            scope.repository,
            scope.subject,
        );
        for activity in activities {
            let delivery = activity.delivery_id();
            let delivered = state
                .contains(
                    scope.repository,
                    scope.object,
                    delivery.operation,
                    delivery.action,
                )
                .await?;
            if !delivered {
                pending.add(activity);
            }
        }
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
    skipped: Vec<Activity>,
    activities: Vec<Activity>,
}

impl PendingActivities {
    fn add(&mut self, activity: Activity) {
        match activity.kind {
            Kind::Assigned => {
                if let Some(superseded) = self.assignment.replace(activity) {
                    self.skipped.push(superseded);
                }
            }
            _ => self.activities.push(activity),
        }
    }

    async fn dispatch(
        self,
        assigned: bool,
        actionable: bool,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        for activity in self.skipped {
            Self::checkpoint(activity, scope, state).await?;
        }
        if let Some(activity) = self.assignment {
            if assigned && actionable {
                Self::deliver(activity, scope, state, agent).await?;
            } else {
                Self::checkpoint(activity, scope, state).await?;
            }
        }
        for activity in self.activities {
            Self::deliver(activity, scope, state, agent).await?;
        }
        Ok(())
    }

    async fn deliver(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        let delivery = activity.delivery_id();
        Self::process_outcome(activity, agent).await?;
        Self::checkpoint_delivery(delivery, scope, state).await
    }

    async fn checkpoint(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
    ) -> Result<(), Error> {
        let delivery = activity.delivery_id();
        Self::checkpoint_delivery(delivery, scope, state).await
    }

    async fn checkpoint_delivery(
        delivery: crate::agent::DeliveryId,
        scope: ScanScope,
        state: &ProcessedActivities,
    ) -> Result<(), Error> {
        let _inserted = state
            .insert(
                scope.repository,
                scope.object,
                delivery.operation,
                delivery.action,
            )
            .await?;
        Ok(())
    }

    async fn process_outcome(
        activity: Activity,
        agent: &mut (impl Agent + ?Sized),
    ) -> Result<(), Error> {
        match activity.dispatch(agent).await? {
            Outcome::NoAction => Ok(()),
            Outcome::Reply(_) => Err(Error::UnsupportedOutcome),
        }
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
    Database(String),
    /// An agent adapter failed while handling classified activity.
    #[error(transparent)]
    Agent(#[from] agent::Error),
    /// The agent requested an action which the controller cannot publish yet.
    #[error("the agent requested an unsupported repository action")]
    UnsupportedOutcome,
    /// Preparing the local watcher database failed.
    #[error("watcher state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The node closed its event stream.
    #[error("Radicle node closed its event stream")]
    EventStreamClosed,
    /// A history producer stopped because its consumer was closed.
    #[error("Radicle history stream consumer closed")]
    HistoryStreamClosed,
    /// The internal history producer emitted an invalid sequence.
    #[error("invalid Radicle history stream: {0}")]
    HistoryProtocol(&'static str),
    /// A blocking Radicle operation task failed.
    #[error("watcher task failed: {0}")]
    Task(String),
}

impl From<crate::database::Error> for Error {
    fn from(error: crate::database::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(miri))]
    use std::io;
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

    #[cfg(not(miri))]
    use crate::{
        agent::{self, Agent, Context as AgentContext, DeliveryId, Outcome, Target},
        watcher::state::ProcessedActivities,
    };
    use crate::{
        member::Member,
        watcher::{
            Change, Comment, Subject,
            activity::{Activity, Kind},
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

    #[cfg(not(miri))]
    struct RejectingAgent {
        mentions: usize,
        reject: usize,
        deliveries: Vec<DeliveryId>,
    }

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl Agent for RejectingAgent {
        async fn assigned(&mut self, _context: AgentContext) -> Result<Outcome, agent::Error> {
            Ok(Outcome::NoAction)
        }

        async fn mentioned(
            &mut self,
            context: AgentContext,
            _target: Target,
        ) -> Result<Outcome, agent::Error> {
            self.mentions += 1;
            self.deliveries.push(context.delivery);
            if self.mentions == self.reject {
                Err(agent::Error::new(io::Error::other("rejected")))
            } else {
                Ok(Outcome::NoAction)
            }
        }

        async fn replied(
            &mut self,
            _context: AgentContext,
            _comment: Comment,
            _reply_to: EntryId,
        ) -> Result<Outcome, agent::Error> {
            Ok(Outcome::NoAction)
        }

        async fn changed(
            &mut self,
            _context: AgentContext,
            _change: Change,
        ) -> Result<Outcome, agent::Error> {
            Ok(Outcome::NoAction)
        }
    }

    impl Fixture {
        fn operation<A>(id: &str, action: A, author: PublicKey, type_name: TypeName) -> Op<A> {
            Self::operation_with(id, action, Vec::new(), author, type_name)
        }

        fn operation_with<A>(
            id: &str,
            action: A,
            additional: Vec<A>,
            author: PublicKey,
            type_name: TypeName,
        ) -> Op<A> {
            Op::new(
                EntryId::from_str(id).expect("operation should be valid"),
                (action, additional),
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

    #[tokio::test]
    #[cfg(not(miri))]
    async fn leaves_rejected_activity_unprocessed_for_retry() {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let object = ObjectId::from_str(ISSUE).expect("issue should be valid");
        let operation = Fixture::operation(
            OPERATION,
            Action::Comment {
                body: "Please investigate, @nico".to_owned(),
                reply_to: None,
                embeds: Vec::new(),
            },
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
            TYPENAME.clone(),
        );
        let scope = super::ScanScope::new(
            Member::Nico,
            PublicKey::from_str(MEMBER).expect("member key should be valid"),
            repository,
            object,
            Subject::Issue(object),
            true,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");

        assert!(matches!(
            super::Watcher::scan_history(
                std::iter::once(Ok(operation)),
                scope,
                &state,
                &mut RejectingAgent {
                    mentions: 0,
                    reject: 1,
                    deliveries: Vec::new(),
                },
            )
            .await,
            Err(super::Error::Agent(_))
        ));
        assert!(
            !state
                .contains(
                    repository,
                    object,
                    EntryId::from_str(OPERATION).expect("operation should be valid"),
                    0,
                )
                .await
                .expect("state should be readable")
        );
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn checkpoints_each_successful_action_before_a_later_failure() {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let object = ObjectId::from_str(ISSUE).expect("issue should be valid");
        let comment = |body: &str| Action::Comment {
            body: body.to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        };
        let operation = Fixture::operation_with(
            OPERATION,
            comment("First request, @nico"),
            vec![comment("Second request, @nico")],
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
            TYPENAME.clone(),
        );
        let operation_id = operation.id;
        let scope = super::ScanScope::new(
            Member::Nico,
            PublicKey::from_str(MEMBER).expect("member key should be valid"),
            repository,
            object,
            Subject::Issue(object),
            true,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        let mut agent = RejectingAgent {
            mentions: 0,
            reject: 2,
            deliveries: Vec::new(),
        };

        assert!(matches!(
            super::Watcher::scan_history(
                std::iter::once(Ok(operation.clone())),
                scope,
                &state,
                &mut agent,
            )
            .await,
            Err(super::Error::Agent(_))
        ));
        assert!(
            state
                .contains(repository, object, operation_id, 0)
                .await
                .expect("first lookup")
        );
        assert!(
            !state
                .contains(repository, object, operation_id, 1)
                .await
                .expect("second lookup")
        );

        super::Watcher::scan_history(std::iter::once(Ok(operation)), scope, &state, &mut agent)
            .await
            .expect("retry should finish");
        assert_eq!(agent.mentions, 3);
        assert_eq!(
            agent
                .deliveries
                .iter()
                .map(|delivery| delivery.action)
                .collect::<Vec<_>>(),
            [0, 1, 1]
        );
        assert!(
            state
                .contains(repository, object, operation_id, 1)
                .await
                .expect("second lookup")
        );
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
