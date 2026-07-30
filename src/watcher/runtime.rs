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
    agent::{self, Agent},
    mcp::DeliveryScope,
    member::Member,
    watcher::{
        activity::{Activity, Kind, Subject},
        classification::{ActionEnvelope, Context as ClassificationContext, NormalizedAction},
        state::ProcessedActivities,
    },
    workspace::Workspaces,
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
        workspaces: Workspaces,
        delivery_scope: DeliveryScope,
    ) -> Result<(), Error> {
        let member_id = Self::verify_profile(*profile.id(), node.clone()).await?;
        let storage = profile.storage.clone();
        let state = ProcessedActivities::open(state_path).await?;
        let events = self.subscribe(node.clone()).await?;
        let delivery = DeliveryRuntime {
            workspaces,
            scope: delivery_scope,
        };
        self.watch(&storage, member_id, &state, events, agent, &delivery)
            .await
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
        delivery: &DeliveryRuntime,
    ) -> Result<(), Error> {
        self.scan_all(storage, member_id, state, agent, delivery)
            .await?;
        loop {
            Self::await_update(&mut events).await?;
            self.scan_all(storage, member_id, state, agent, delivery)
                .await?;
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
        delivery: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let issues = self.issue_histories(storage.clone());
        self.scan_objects(issues, member_id, state, agent, delivery, Subject::Issue)
            .await?;
        let patches = self.patch_histories(storage.clone());
        self.scan_objects(patches, member_id, state, agent, delivery, Subject::Patch)
            .await
    }

    async fn scan_objects<A>(
        &self,
        mut histories: tokio::sync::mpsc::Receiver<HistoryEvent<A>>,
        member_id: NodeId,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        delivery: &DeliveryRuntime,
        subject: fn(ObjectId) -> Subject,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut active = None;
        let source = HistorySource { member_id, subject };
        while let Some(event) = histories.recv().await {
            self.handle_history_event(event, source, &mut active, state, agent, delivery)
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
        source: HistorySource,
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        delivery: &DeliveryRuntime,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        match event {
            HistoryEvent::Begin { object, actionable } => {
                self.begin_history(object, actionable, source.member_id, source.subject, active)
            }
            HistoryEvent::Operation(operation) => Self::scan_active(operation, active, state).await,
            HistoryEvent::End => Self::finish_active(active, state, agent, delivery).await,
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
        delivery: &DeliveryRuntime,
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
                delivery,
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

#[derive(Debug, Clone)]
struct DeliveryRuntime {
    workspaces: Workspaces,
    scope: DeliveryScope,
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

#[derive(Clone, Copy)]
struct HistorySource {
    member_id: NodeId,
    subject: fn(ObjectId) -> Subject,
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
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        for activity in self.skipped {
            Self::checkpoint(activity, scope, state, runtime).await?;
        }
        if let Some(activity) = self.assignment {
            if assigned && actionable {
                Self::deliver(activity, scope, state, agent, runtime).await?;
            } else {
                Self::checkpoint(activity, scope, state, runtime).await?;
            }
        }
        for activity in self.activities {
            Self::deliver(activity, scope, state, agent, runtime).await?;
        }
        Ok(())
    }

    async fn deliver(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        Self::deliver_fresh(activity, scope, state, agent, runtime).await
    }

    async fn deliver_fresh(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let delivery = activity.delivery_id();
        let subject = activity.subject;
        let workspace = runtime
            .workspaces
            .prepare(delivery, subject)
            .await
            .map_err(|error| Error::Workspace(error.to_string()))?;
        let path = workspace.path().to_path_buf();
        runtime
            .scope
            .activate(activity.context(path.clone()))
            .await
            .map_err(Error::DeliveryScope)?;
        let handled = activity.dispatch(agent, path).await;
        runtime.scope.deactivate().await;
        let cleaned = workspace.cleanup().await;
        handled?;
        cleaned.map_err(|error| Error::Workspace(error.to_string()))?;
        Self::checkpoint_delivery(delivery, scope, state, runtime).await
    }

    async fn checkpoint(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let delivery = activity.delivery_id();
        Self::checkpoint_delivery(delivery, scope, state, runtime).await
    }

    async fn checkpoint_delivery(
        delivery: crate::agent::DeliveryId,
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        runtime
            .scope
            .reconcile(delivery)
            .await
            .map_err(Error::PublicationRecovery)?;
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
}

fn send_history<A>(
    sender: &tokio::sync::mpsc::Sender<HistoryEvent<A>>,
    event: HistoryEvent<A>,
) -> Result<(), Error> {
    sender
        .blocking_send(event)
        .map_err(|_send| Error::HistoryStreamClosed)
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
    /// Preparing the local watcher database failed.
    #[error("watcher state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Preparing or cleaning the agent workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the delivery-scoped MCP capability failed.
    #[error("delivery scope failed: {0}")]
    DeliveryScope(String),
    /// Recovering durable MCP publications before checkpointing failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
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

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        cob::{EntryId, ObjectId},
        crypto::Seed,
        identity::RepoId,
        node::Alias,
        profile::{Home, Profile},
    };

    use crate::{
        agent::{Agent, Context, McpEndpoint},
        member::{Member, Profile as MemberProfile},
        watcher::{
            Change, Comment, Subject, Target,
            activity::{Activity, Kind},
            runtime::{DeliveryRuntime, PendingActivities, ScanScope},
            state::ProcessedActivities,
        },
        workspace::Workspaces,
    };

    #[tokio::test]
    async fn checkpoints_superseded_and_inactive_assignments() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let profile = Profile::init(
            Home::new(root.path()).expect("home should be created"),
            Alias::new("dispatch-test"),
            None,
            Seed::generate(),
        )
        .expect("profile should initialize");
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository should be valid");
        let object = ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("object should be valid");
        let first = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should be valid");
        let second = EntryId::from_str("3333333333333333333333333333333333333333")
            .expect("operation should be valid");
        let activity = |operation| {
            Activity::new(
                repository,
                Subject::Issue(object),
                operation,
                0,
                profile.did(),
                Kind::Assigned,
            )
        };
        let mut pending = PendingActivities::default();
        pending.add(activity(first));
        pending.add(activity(second));
        let scope = ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            object,
            Subject::Issue(object),
            false,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should initialize");
        let runtime = DeliveryRuntime {
            workspaces: Workspaces::new(
                root.path().join("workspaces"),
                profile.storage.clone(),
                repository,
                *profile.id(),
            ),
            scope: crate::mcp::DeliveryScope::default(),
        };

        pending
            .dispatch(false, false, scope, &state, &mut NoopAgent, &runtime)
            .await
            .expect("assignments should be checkpointed");
        for operation in [first, second] {
            assert!(
                state
                    .contains(repository, object, operation, 0)
                    .await
                    .expect("state should be readable")
            );
        }
    }

    struct NoopAgent;

    #[async_trait::async_trait]
    impl Agent for NoopAgent {
        async fn start(
            &mut self,
            _profile: MemberProfile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
        ) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn assigned(&mut self, _context: Context) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn mentioned(
            &mut self,
            _context: Context,
            _target: Target,
        ) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn replied(
            &mut self,
            _context: Context,
            _comment: Comment,
            _reply_to: EntryId,
        ) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn changed(
            &mut self,
            _context: Context,
            _change: Change,
        ) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), crate::agent::Error> {
            Ok(())
        }
    }
}
