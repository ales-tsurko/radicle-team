//! Watcher lifecycle, scanning, and event-stream orchestration.

use std::path::Path;

use radicle::{
    cob::{
        Label, ObjectId, Op, Timestamp,
        issue::{Action as IssueAction, IssueStream, Issues, State as IssueState},
        patch::{Action as PatchAction, PatchStream, Patches, State as PatchState},
        store::access::ReadOnly,
        stream::CobStream as _,
    },
    crypto::PublicKey,
    identity::{Did, RepoId},
    node::{Handle as _, Node, NodeId},
    profile::Profile as RadicleProfile,
    storage::{ReadRepository as _, ReadStorage as _, git::Storage},
};

use crate::{
    agent,
    conversation::{self, Triggers},
    manifest::{PriorityLabels, QueuePosition, RepositoryPolicy},
    member::Member,
    queue::TaskQueue,
    watcher::{
        activity::{Activity, Delivery, Kind, Subject},
        classification::{ActionEnvelope, Context as ClassificationContext, NormalizedAction},
        events::NodeEvents,
        state::ProcessedActivities,
    },
};

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
    policy: RepositoryPolicy,
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
    pub const fn new(member: Member, repository: RepoId, policy: RepositoryPolicy) -> Self {
        Self {
            member,
            repository,
            policy,
        }
    }

    /// Watches with an identity profile already validated by the controller.
    pub(crate) async fn run(
        &self,
        profile: &RadicleProfile,
        node: &Node,
        started: Timestamp,
        state_path: &Path,
        delivery: DeliveryRuntime,
    ) -> Result<(), Error> {
        let member_id = Self::verify_profile(*profile.id(), node.clone()).await?;
        let storage = profile.storage.clone();
        let state = ProcessedActivities::open(state_path).await?;
        let mut events = NodeEvents::subscribe(self.repository, node.clone()).await?;
        tokio::select! {
            watched = self.watch(
                &storage, member_id, &state, started, events.changes, &delivery,
            ) => watched,
            failed = &mut events.failure => {
                Err(failed.map_err(|error| Error::Task(error.to_string()))?)
            }
        }
    }

    async fn verify_profile(member_id: PublicKey, node: Node) -> Result<NodeId, Error> {
        let node_id = tokio::task::spawn_blocking(move || node.nid())
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Self::verify_identity(member_id, node_id)?;

        Ok(member_id)
    }

    async fn watch(
        &self,
        storage: &Storage,
        member_id: PublicKey,
        state: &ProcessedActivities,
        started: Timestamp,
        events: tokio::sync::watch::Receiver<()>,
        delivery: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let started = state.started(self.repository, started).await?;
        let scan = |mode, mut updates: tokio::sync::watch::Receiver<()>| async move {
            loop {
                updates.borrow_and_update();
                self.scan_all(storage, member_id, state, delivery, mode)
                    .await?;
                if updates.changed().await.is_err() {
                    return Err::<(), Error>(Error::EventStreamClosed);
                }
            }
        };
        tokio::try_join!(
            scan(ScanMode::Assignments, events.clone()),
            scan(ScanMode::Conversations(started), events),
        )?;
        Ok(())
    }

    async fn scan_all(
        &self,
        storage: &Storage,
        member_id: PublicKey,
        state: &ProcessedActivities,
        delivery: &DeliveryRuntime,
        mode: ScanMode,
    ) -> Result<(), Error> {
        let triggers = self.resolve_triggers(storage).await?;
        tracing::debug!(
            member = %self.member,
            repository = %self.repository,
            ?mode,
            "scanning repository histories"
        );
        let issues = self.issue_histories(storage.clone());
        self.scan_objects(
            issues,
            state,
            delivery,
            HistorySource::new(member_id, Subject::Issue, &triggers, mode),
        )
        .await?;
        let patches = self.patch_histories(storage.clone());
        self.scan_objects(
            patches,
            state,
            delivery,
            HistorySource::new(member_id, Subject::Patch, &triggers, mode),
        )
        .await
    }

    /// Resolves the repository's current delegate set for `access`.
    ///
    /// Delegates are re-read on every scan so that adding or removing one takes effect without
    /// restarting the member process.
    async fn resolve_triggers(&self, storage: &Storage) -> Result<Triggers, Error> {
        let storage = storage.clone();
        let repository = self.repository;
        let delegates = tokio::task::spawn_blocking(move || {
            let repository = storage.repository(repository)?;
            Ok::<Vec<Did>, Error>(repository.delegates()?.into_iter().collect())
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;

        Ok(Triggers::new(
            self.policy.access().clone(),
            delegates,
            self.policy.members().to_vec(),
        ))
    }

    async fn scan_objects<A>(
        &self,
        mut histories: tokio::sync::mpsc::Receiver<HistoryEvent<A>>,
        state: &ProcessedActivities,
        delivery: &DeliveryRuntime,
        source: HistorySource<'_>,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut active = None;
        while let Some(event) = histories.recv().await {
            self.handle_history_event(event, source, &mut active, state, delivery)
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
        source: HistorySource<'_>,
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
        delivery: &DeliveryRuntime,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        match event {
            HistoryEvent::Begin { object, actionable } => {
                self.begin_history(object, actionable, source.member_id, source.subject, active)
            }
            HistoryEvent::Operation(operation) => {
                Self::scan_active(operation, active, state, source.triggers, source.mode).await
            }
            HistoryEvent::End => Self::finish_active(active, state, delivery).await,
            HistoryEvent::Failed(error) => Err(error),
        }
    }

    fn begin_history(
        &self,
        object: ObjectId,
        actionable: bool,
        member_id: PublicKey,
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
            context: ClassificationContext::new(self.policy.limits().clone()),
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
        triggers: &Triggers,
        mode: ScanMode,
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
            triggers,
            mode,
        )
        .await
    }

    async fn finish_active(
        active: &mut Option<ActiveHistory>,
        state: &ProcessedActivities,
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
        triggers: &Triggers,
        mode: ScanMode,
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
            triggers,
        );
        if matches!(mode, ScanMode::Conversations(started) if operation.timestamp < started) {
            // The cutoff comes from node startup, not the time this scan encounters an operation.
            // Apply it on every rescan, including history fetched after initialization. Record
            // ignored actions too so changing delegates cannot revive historical conversations.
            // Current open assignments still go through the independent durable task queue.
            for action in 0..operation.actions.len() {
                if !activities
                    .iter()
                    .any(|activity| activity.action == action && activity.kind == Kind::Assigned)
                {
                    let _inserted = state
                        .insert(scope.repository, scope.object, operation.id, action)
                        .await?;
                }
            }
        }
        for activity in activities {
            if !mode.accepts(activity.kind) {
                continue;
            }
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

    fn verify_identity(profile: NodeId, node: NodeId) -> Result<(), Error> {
        if profile == node {
            Ok(())
        } else {
            Err(Error::IdentityMismatch { profile, node })
        }
    }
}

// Everything one scan needs in order to hand work to the member's roles.
#[derive(Debug, Clone)]
pub(crate) struct DeliveryRuntime {
    storage: Storage,
    queue: TaskQueue,
    priority: PriorityLabels,
    conversations: conversation::Handle,
}

impl DeliveryRuntime {
    // Pairs classified deliveries with the conversation dispatcher and assignment queue.
    pub(crate) const fn new(
        storage: Storage,
        queue: TaskQueue,
        priority: PriorityLabels,
        conversations: conversation::Handle,
    ) -> Self {
        Self {
            storage,
            queue,
            priority,
            conversations,
        }
    }

    // Reads the assigned subject as a task the building role can start from.
    async fn assignment(&self, repository: RepoId, subject: Subject) -> Result<Assignment, Error> {
        let storage = self.storage.clone();
        let priority = self.priority.clone();
        tokio::task::spawn_blocking(move || {
            let stored = storage.repository(repository)?;
            let id = subject.id();
            let missing = || Error::SubjectMissing(subject);
            match subject {
                Subject::Issue(_) => {
                    let issue = Issues::open(&stored, ReadOnly)?
                        .get(&id)?
                        .ok_or_else(missing)?;
                    Ok(Assignment::new(
                        subject,
                        issue.title(),
                        issue.description(),
                        issue.labels(),
                        &priority,
                    ))
                }
                Subject::Patch(_) => {
                    let patch = Patches::open(&stored, ReadOnly)?
                        .get(&id)?
                        .ok_or_else(missing)?;
                    Ok(Assignment::new(
                        subject,
                        patch.title(),
                        patch.description(),
                        patch.labels(),
                        &priority,
                    ))
                }
            }
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
    }
}

struct Assignment {
    brief: String,
    position: QueuePosition,
}

impl Assignment {
    fn new<'a>(
        subject: Subject,
        title: &str,
        description: &str,
        labels: impl Iterator<Item = &'a Label>,
        priority: &PriorityLabels,
    ) -> Self {
        let mut brief = format!("You were assigned this {}: {title}", subject.kind());
        let description = description.trim();
        if !description.is_empty() {
            brief.push_str("\n\n");
            brief.push_str(description);
        }

        Self {
            brief,
            position: priority.position(labels.map(Label::name)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    Assignments,
    Conversations(Timestamp),
}

impl ScanMode {
    const fn accepts(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Self::Assignments, Kind::Assigned) | (Self::Conversations(_), Kind::Delivered(_))
        )
    }
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
struct HistorySource<'a> {
    member_id: PublicKey,
    subject: fn(ObjectId) -> Subject,
    triggers: &'a Triggers,
    mode: ScanMode,
}

impl<'a> HistorySource<'a> {
    const fn new(
        member_id: PublicKey,
        subject: fn(ObjectId) -> Subject,
        triggers: &'a Triggers,
        mode: ScanMode,
    ) -> Self {
        Self {
            member_id,
            subject,
            triggers,
            mode,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ScanScope {
    member: Member,
    member_id: PublicKey,
    repository: RepoId,
    object: ObjectId,
    subject: Subject,
    actionable: bool,
}

impl ScanScope {
    const fn new(
        member: Member,
        member_id: PublicKey,
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
    activities: Vec<(Activity, Delivery)>,
}

impl PendingActivities {
    fn add(&mut self, activity: Activity) {
        match activity.kind {
            Kind::Assigned => {
                if let Some(superseded) = self.assignment.replace(activity) {
                    self.skipped.push(superseded);
                }
            }
            Kind::Delivered(delivery) => self.activities.push((activity, delivery)),
        }
    }

    async fn dispatch(
        self,
        assigned: bool,
        actionable: bool,
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        for activity in self.skipped {
            Self::record_delivery(activity.delivery_id(), scope, state).await?;
        }
        if let Some(activity) = self.assignment {
            if assigned && actionable {
                Self::enqueue(activity, scope, state, runtime).await?;
            } else {
                Self::record_delivery(activity.delivery_id(), scope, state).await?;
            }
        }
        for (activity, delivery) in self.activities {
            Self::deliver(activity, delivery, scope, state, runtime).await?;
        }
        Ok(())
    }

    // Records an assignment as queued work rather than handing it to the conversation.
    async fn enqueue(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let subject = activity.subject;
        let assignment = runtime.assignment(scope.repository, subject).await?;
        let position = assignment.position;
        let mut request =
            crate::queue::TaskRequest::new(assignment.brief, subject.into(), position);
        request.request = Some(crate::agent::ActivityId::from(activity.delivery_id()).request());
        request
            .context
            .messages
            .push(crate::queue::MessageReference {
                subject: subject.to_string(),
                entry: activity.operation.to_string(),
                revision: None,
                purpose: "Original assignment".to_owned(),
            });
        let receipt = runtime.queue.push(request).await?;
        tracing::info!(
            member = %scope.member,
            repository = %scope.repository,
            %subject,
            task = receipt.task,
            ?position,
            "assignment recorded as queued work"
        );
        // Assignments do not publish under a talker delivery identity, so acknowledging the
        // durable task must not wait behind a conversation.
        Self::record_delivery(activity.delivery_id(), scope, state).await
    }

    async fn deliver(
        activity: Activity,
        delivery: Delivery,
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
    ) -> Result<(), Error> {
        let identity = activity.delivery_id();
        runtime
            .conversations
            .deliver(activity, delivery)
            .await
            .map_err(Error::from)?;
        Self::record_delivery(identity, scope, state).await
    }

    async fn record_delivery(
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
    /// Recording assigned work failed.
    #[error("task queue error: {0}")]
    Queue(#[from] crate::queue::Error),
    /// An assignment names an issue or patch the repository does not contain.
    #[error("assigned {0} was not found")]
    SubjectMissing(Subject),
    /// An agent adapter failed while handling classified activity.
    #[error(transparent)]
    Talker(#[from] agent::Error),
    /// Preparing the local watcher database failed.
    #[error("watcher state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Preparing or cleaning the agent workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the activity-scoped MCP capability failed.
    #[error("activity scope failed: {0}")]
    ActivityScope(String),
    /// Recovering durable MCP publications before checkpointing failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
    /// The conversation dispatcher stopped accepting deliveries.
    #[error("conversation dispatcher is not running")]
    ConversationUnavailable,
    /// The agent stopped making progress and was killed.
    #[error("agent was killed after making no progress")]
    Stalled,
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

impl From<conversation::Error> for Error {
    fn from(error: conversation::Error) -> Self {
        match error {
            conversation::Error::Talker(error) => Self::Talker(error),
            conversation::Error::Workspace(error) => Self::Workspace(error),
            conversation::Error::ActivityScope(error) => Self::ActivityScope(error),
            conversation::Error::PublicationRecovery(error) => Self::PublicationRecovery(error),
            conversation::Error::Unavailable => Self::ConversationUnavailable,
            conversation::Error::Stalled => Self::Stalled,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr as _,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, UNIX_EPOCH},
    };

    use radicle::{
        cob::{
            EntryId, Label, Manifest, ObjectId, Op, Timestamp, Title, Version,
            issue::{Action as IssueAction, TYPENAME},
        },
        crypto::{PublicKey, Seed},
        identity::{Did, RepoId, doc::Visibility},
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };

    use crate::{
        agent::{Context, McpEndpoint, Talker},
        conversation::{Dispatcher, Triggers},
        manifest::{Limits, PriorityLabels, QueuePosition, RepositoryPolicy, TriggerAccess},
        member::{Member, Profile as MemberProfile},
        queue::TaskQueue,
        watcher::{
            Change, Comment, Subject, Target,
            activity::{Activity, Delivery, Kind},
            classification::Context as ClassificationContext,
            runtime::{DeliveryRuntime, PendingActivities, ScanMode, ScanScope, Watcher},
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
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let (conversations, dispatcher) = Dispatcher::new(
            Member::Nico,
            Workspaces::new(
                Member::Nico,
                crate::home::MemberHome::at(root.path()),
                profile.storage.clone(),
                repository,
                profile.info(),
                queue.clone(),
            ),
            crate::mcp::ActivityScope::default(),
        );
        let runtime = DeliveryRuntime::new(
            profile.storage.clone(),
            queue.clone(),
            PriorityLabels::default(),
            conversations,
        );

        dispatch_pending(
            pending,
            (false, false),
            scope,
            &state,
            &runtime,
            dispatcher,
            false,
        )
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
        assert!(
            queue
                .pending()
                .await
                .expect("queue should be readable")
                .is_empty(),
            "an assignment that never became active must not leave work behind"
        );
    }

    /// Builds a repository holding one labelled issue, and returns its identifiers.
    fn assigned_issue(root: &std::path::Path, labels: &[Label]) -> (Profile, RepoId, ObjectId) {
        let profile = Profile::init(
            Home::new(root.join("home")).expect("home should be created"),
            Alias::new("assignment-test"),
            None,
            Seed::generate(),
        )
        .expect("profile should initialize");
        let working = root.join("working");
        let checkout = git2::Repository::init(&working).expect("repository should initialize");
        let signature = git2::Signature::now("Assignment Test", "assignment@example.com")
            .expect("signature should be valid");
        std::fs::write(working.join("README.md"), "initial\n").expect("README should be written");
        let tree = {
            let mut index = checkout.index().expect("index should open");
            index
                .add_path(std::path::Path::new("README.md"))
                .expect("README should be added");
            let oid = index.write_tree().expect("tree should be written");
            checkout.find_tree(oid).expect("tree should exist")
        };
        checkout
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Initial",
                &tree,
                &[],
            )
            .expect("commit should be created");
        drop(tree);
        let signer = profile.signer().expect("signer should load");
        let (repository, _document, _refs) = radicle::rad::init(
            &checkout,
            "assignment-test"
                .try_into()
                .expect("project name should be valid"),
            "Assignment test",
            "main".try_into().expect("branch should be valid"),
            Visibility::default(),
            &signer,
            &profile.storage,
        )
        .expect("Radicle repository should initialize");
        let issue = {
            let stored = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let mut issues = profile
                .issues_mut(&stored, &signer)
                .expect("issue cache should open");
            let issue = issues
                .create(
                    Title::try_from("Fix the parser".to_owned()).expect("title should be valid"),
                    "It drops the last field.",
                    labels,
                    &[],
                    [],
                )
                .expect("issue should be created");
            *issue.id()
        };

        (profile, repository, issue)
    }

    #[tokio::test]
    async fn checkpoints_a_delivery_only_after_the_dispatcher_handles_it() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let (profile, repository, issue) = assigned_issue(root.path(), &[]);
        let operation = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should be valid");
        let pending = || {
            let mut pending = PendingActivities::default();
            pending.add(Activity::new(
                repository,
                Subject::Issue(issue),
                operation,
                0,
                profile.did(),
                Kind::Delivered(Delivery::Mentioned(Target::Subject)),
            ));
            pending
        };
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let home = crate::home::MemberHome::at(root.path().join("member"));
        home.create().expect("member directory should be created");
        let workspaces = Workspaces::new(
            Member::Nico,
            home,
            profile.storage.clone(),
            repository,
            profile.info(),
            queue.clone(),
        );
        workspaces
            .initialize()
            .await
            .expect("working copies should be prepared");
        let delivery = || {
            let (conversations, dispatcher) = Dispatcher::new(
                Member::Nico,
                workspaces.clone(),
                crate::mcp::ActivityScope::default(),
            );
            let runtime = DeliveryRuntime::new(
                profile.storage.clone(),
                queue.clone(),
                PriorityLabels::default(),
                conversations,
            );
            (runtime, dispatcher)
        };
        let state = ProcessedActivities::memory()
            .await
            .expect("state should initialize");
        let scope = ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue,
            Subject::Issue(issue),
            true,
        );

        let (runtime, dispatcher) = delivery();
        let failed = dispatch_pending(
            pending(),
            (false, true),
            scope,
            &state,
            &runtime,
            dispatcher,
            true,
        )
        .await;
        assert!(matches!(failed, Err(super::Error::Talker(_))));
        assert!(
            !state
                .contains(repository, issue, operation, 0)
                .await
                .expect("state should be readable"),
            "a failed delivery must remain available for retry"
        );

        let (runtime, dispatcher) = delivery();
        dispatch_pending(
            pending(),
            (false, true),
            scope,
            &state,
            &runtime,
            dispatcher,
            false,
        )
        .await
        .expect("delivery should be handled");

        assert!(
            state
                .contains(repository, issue, operation, 0)
                .await
                .expect("state should be readable"),
            "the watcher must checkpoint the delivery after the dispatcher acknowledges it"
        );
    }

    async fn enqueue_assignment(labels: &[Label]) -> (TaskQueue, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let (profile, repository, issue) = assigned_issue(root.path(), labels);
        let operation = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should be valid");
        let mut pending = PendingActivities::default();
        pending.add(Activity::new(
            repository,
            Subject::Issue(issue),
            operation,
            0,
            profile.did(),
            Kind::Assigned,
        ));
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let (conversations, dispatcher) = Dispatcher::new(
            Member::Nico,
            Workspaces::new(
                Member::Nico,
                crate::home::MemberHome::at(root.path()),
                profile.storage.clone(),
                repository,
                profile.info(),
                queue.clone(),
            ),
            crate::mcp::ActivityScope::default(),
        );
        let runtime = DeliveryRuntime::new(
            profile.storage.clone(),
            queue.clone(),
            PriorityLabels::default(),
            conversations,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should initialize");
        let scope = ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue,
            Subject::Issue(issue),
            true,
        );
        dispatch_pending(
            pending,
            (true, true),
            scope,
            &state,
            &runtime,
            dispatcher,
            false,
        )
        .await
        .expect("an active assignment should become queued work");
        assert!(
            state
                .contains(repository, issue, operation, 0)
                .await
                .expect("state should be readable"),
            "a queued assignment must be checkpointed, or a rescan would queue it again"
        );

        (queue, root)
    }

    #[tokio::test]
    async fn records_an_assignment_as_work_rather_than_conversation() {
        let (queue, _root) = enqueue_assignment(&[]).await;

        let pending = queue.pending().await.expect("queue should be readable");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].position,
            QueuePosition::End,
            "work nobody marked urgent joins the end of the queue"
        );
        assert!(
            pending[0].brief.contains("Fix the parser")
                && pending[0].brief.contains("It drops the last field."),
            "the brief must carry the request, so the building role can start without rereading \
             the issue: {}",
            pending[0].brief
        );
    }

    #[tokio::test]
    async fn takes_the_queue_position_from_the_labels_on_the_subject() {
        for (label, expected) in [
            ("urgent", QueuePosition::Front),
            ("next", QueuePosition::Next),
            ("chore", QueuePosition::End),
        ] {
            let label = Label::new(label).expect("label should be valid");
            let (queue, _root) = enqueue_assignment(std::slice::from_ref(&label)).await;

            let pending = queue.pending().await.expect("queue should be readable");
            assert_eq!(
                pending[0].position, expected,
                "'{label}' must decide where the assignment joins the queue"
            );
        }
    }

    struct RuntimeFixture {
        _root: tempfile::TempDir,
        profile: Profile,
        repository: RepoId,
        issue: ObjectId,
        member_id: PublicKey,
        queue: TaskQueue,
        state: ProcessedActivities,
        watcher: Watcher,
        delivery: DeliveryRuntime,
        dispatcher: Option<Dispatcher>,
    }

    impl RuntimeFixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let (profile, repository, issue) = assigned_issue(root.path(), &[]);
            let member_id = PublicKey::from_str("z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7")
                .expect("member key should be valid");
            let queue = TaskQueue::memory(repository, Member::Nico)
                .await
                .expect("queue should open");
            let state = ProcessedActivities::open(&root.path().join("state.db"))
                .await
                .expect("state should open");
            let home = crate::home::MemberHome::at(root.path().join("member"));
            home.create().expect("home should be created");
            let workspaces = Workspaces::new(
                Member::Nico,
                home,
                profile.storage.clone(),
                repository,
                profile.info(),
                queue.clone(),
            );
            workspaces
                .initialize()
                .await
                .expect("working copies should be prepared");
            let (conversations, dispatcher) = Dispatcher::new(
                Member::Nico,
                workspaces,
                crate::mcp::ActivityScope::default(),
            );
            let delivery = DeliveryRuntime::new(
                profile.storage.clone(),
                queue.clone(),
                PriorityLabels::default(),
                conversations,
            );
            let watcher = Watcher::new(
                Member::Nico,
                repository,
                RepositoryPolicy::new(
                    TriggerAccess::Anyone,
                    vec![Did::from(member_id)],
                    PriorityLabels::default(),
                    Limits::default(),
                ),
            );
            Self {
                _root: root,
                profile,
                repository,
                issue,
                member_id,
                queue,
                state,
                watcher,
                delivery,
                dispatcher: Some(dispatcher),
            }
        }

        fn comment(&self, body: &str) -> EntryId {
            let signer = self.profile.signer().expect("signer should load");
            let stored = self
                .profile
                .storage
                .repository(self.repository)
                .expect("stored repository should open");
            let mut issues = self
                .profile
                .issues_mut(&stored, &signer)
                .expect("issues should open");
            issues
                .get_mut(&self.issue)
                .expect("issue should open")
                .comment(body, *self.issue, [])
                .expect("comment should be created")
        }

        fn assign_existing_issue(&self) {
            let signer = self.profile.signer().expect("signer should load");
            let stored = self
                .profile
                .storage
                .repository(self.repository)
                .expect("stored repository should open");
            let mut issues = self
                .profile
                .issues_mut(&stored, &signer)
                .expect("issues should open");
            issues
                .get_mut(&self.issue)
                .expect("issue should open")
                .assign([Did::from(self.member_id)])
                .expect("issue should be assigned");
        }

        fn create_assignment(&self) -> ObjectId {
            let signer = self.profile.signer().expect("signer should load");
            let stored = self
                .profile
                .storage
                .repository(self.repository)
                .expect("stored repository should open");
            let mut issues = self
                .profile
                .issues_mut(&stored, &signer)
                .expect("issues should open");
            *issues
                .create(
                    Title::try_from("Implement CRUD operations".to_owned())
                        .expect("title should be valid"),
                    "add, done, delete and edit",
                    &[],
                    &[Did::from(self.member_id)],
                    [],
                )
                .expect("issue should be created")
                .id()
        }

        async fn wait_for_tasks(&self, count: usize) {
            while self
                .queue
                .waiting()
                .await
                .expect("queue should be readable")
                != count
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    #[tokio::test]
    async fn fresh_state_skips_old_replies_and_discovers_tasks_during_a_slow_reply() {
        let mut fixture = RuntimeFixture::new().await;
        let historical = fixture.comment("@nico please inspect this");
        fixture.assign_existing_issue();
        let cutoff = Timestamp::from_secs(
            UNIX_EPOCH
                .elapsed()
                .expect("clock should be after the epoch")
                .as_secs()
                + 1,
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        // This mention arrives after node startup, before the watcher finishes initialization.
        let current = fixture.comment("@nico please explain the new request");
        let (updates, events) = tokio::sync::watch::channel(());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut agent = NoopAgent {
            release: Some(release.clone()),
            ..NoopAgent::default()
        };
        let calls = agent.calls.clone();
        let reply_started = agent.started.clone();
        let dispatcher = fixture.dispatcher.take().expect("dispatcher should exist");
        let exercise = async {
            reply_started.notified().await;
            fixture.wait_for_tasks(1).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "only the mention created during initialization should start the talker"
            );
            assert!(
                fixture
                    .state
                    .contains(fixture.repository, fixture.issue, historical, 0)
                    .await
                    .expect("historical checkpoint should be readable")
            );

            assert!(
                !fixture
                    .state
                    .contains(fixture.repository, fixture.issue, current, 0)
                    .await
                    .expect("current checkpoint should be readable")
            );

            let new_issue = fixture.create_assignment();
            updates.send(()).expect("scanners should be notified");
            fixture.wait_for_tasks(2).await;
            let tasks = fixture
                .queue
                .pending()
                .await
                .expect("tasks should be readable");
            assert!(
                tasks
                    .iter()
                    .any(|task| task.source == Subject::Issue(new_issue).into()
                        && task.brief.contains("add, done, delete and edit"))
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the first reply must still be active"
            );
            assert!(
                !fixture
                    .state
                    .contains(fixture.repository, fixture.issue, current, 0)
                    .await
                    .expect("a slow reply must remain unacknowledged")
            );

            release.notify_one();
            while !fixture
                .state
                .contains(fixture.repository, fixture.issue, current, 0)
                .await
                .expect("completed reply should be readable")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "rescans must not duplicate a reply"
            );
        };
        let watching = fixture.watcher.watch(
            &fixture.profile.storage,
            fixture.member_id,
            &fixture.state,
            cutoff,
            events,
            &fixture.delivery,
        );
        tokio::pin!(watching);
        let dispatching = dispatcher.run(&mut agent);
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                () = exercise => Ok(()),
                result = watching => result.and(Err(super::Error::EventStreamClosed)),
                result = dispatching => Err(result.err().map(super::Error::from)
                    .unwrap_or(super::Error::ConversationUnavailable)),
            }
        })
        .await
        .expect("task discovery must not wait for the slow reply");
        outcome.expect("the watcher and dispatcher should stay running");
    }

    #[tokio::test]
    async fn startup_cutoff_uses_creation_time_on_every_rescan() {
        let fixture = RuntimeFixture::new().await;
        let scope = ScanScope::new(
            Member::Nico,
            fixture.member_id,
            fixture.repository,
            fixture.issue,
            Subject::Issue(fixture.issue),
            true,
        );
        let triggers = Triggers::new(TriggerAccess::Anyone, Vec::new(), Vec::new());
        let cutoff = Timestamp::from_secs(100);
        for _rescan in 0..2 {
            let mut context = ClassificationContext::new(Limits::default());
            let mut pending = PendingActivities::default();
            for (id, seconds) in [
                ("1111111111111111111111111111111111111111", 99),
                ("2222222222222222222222222222222222222222", 100),
                ("3333333333333333333333333333333333333333", 101),
            ] {
                let operation = Op::new(
                    id.parse().expect("operation ID should be valid"),
                    (
                        IssueAction::Comment {
                            body: "@nico please inspect this".to_owned(),
                            reply_to: None,
                            embeds: Vec::new(),
                        },
                        Vec::new(),
                    ),
                    *fixture.profile.id(),
                    Timestamp::from_secs(seconds),
                    None,
                    Manifest::new(TYPENAME.clone(), Version::default()),
                );
                let entry = operation.id;
                Watcher::scan_operation(
                    operation,
                    scope,
                    &mut context,
                    &mut pending,
                    &fixture.state,
                    &triggers,
                    ScanMode::Conversations(cutoff),
                )
                .await
                .expect("operation should be scanned");
                assert_eq!(
                    fixture
                        .state
                        .contains(fixture.repository, fixture.issue, entry, 0)
                        .await
                        .expect("checkpoint should be readable"),
                    seconds < 100,
                    "old operations fetched later must be skipped; startup-second operations stay \
                     live"
                );
            }
            assert_eq!(
                pending.activities.len(),
                2,
                "the startup-second and later mentions must still await successful delivery"
            );
        }
    }

    #[derive(Default)]
    struct NoopAgent {
        fail_mentions: bool,
        calls: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    async fn dispatch_pending(
        pending: PendingActivities,
        state_flags: (bool, bool),
        scope: ScanScope,
        state: &ProcessedActivities,
        runtime: &DeliveryRuntime,
        dispatcher: Dispatcher,
        fail_mentions: bool,
    ) -> Result<(), super::Error> {
        let mut agent = NoopAgent {
            fail_mentions,
            ..NoopAgent::default()
        };
        let (assigned, actionable) = state_flags;
        let pending = pending.dispatch(assigned, actionable, scope, state, runtime);
        let dispatching = dispatcher.run(&mut agent);
        tokio::pin!(pending);
        tokio::pin!(dispatching);
        tokio::select! {
            result = pending => result,
            result = dispatching => match result {
                Ok(()) => Err(super::Error::ConversationUnavailable),
                Err(error) => Err(error.into()),
            },
        }
    }

    impl crate::agent::Supervised for NoopAgent {
        fn supervisor(&self) -> std::sync::Arc<crate::agent::Session> {
            std::sync::Arc::new(crate::agent::Session::new(crate::agent::Role::Talker))
        }
    }

    #[async_trait::async_trait]
    impl Talker for NoopAgent {
        async fn start(
            &mut self,
            _profile: MemberProfile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
            _layout: crate::agent::Layout,
        ) -> Result<(), crate::agent::Error> {
            Ok(())
        }

        async fn mentioned(
            &mut self,
            _context: Context,
            _target: Target,
        ) -> Result<(), crate::agent::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            if self.fail_mentions {
                Err(crate::agent::Error::new(std::io::Error::other(
                    "expected test failure",
                )))
            } else {
                Ok(())
            }
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

        async fn shutdown(&mut self) -> Result<(), crate::agent::Error> {
            Ok(())
        }
    }
}
