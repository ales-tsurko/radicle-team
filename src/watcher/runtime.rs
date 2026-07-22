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
    crypto::{PublicKey, ssh::keystore::MemorySigner},
    identity::RepoId,
    node::{Handle as _, Node, NodeId, events::Event as NodeEvent},
    profile::Profile as RadicleProfile,
    storage::{ReadStorage as _, RefUpdate, git::Storage},
};

use crate::{
    agent::{self, Agent, Outcome, Publisher},
    member::Member,
    watcher::{
        activity::{Activity, Kind, Subject},
        classification::{ActionEnvelope, Context as ClassificationContext, NormalizedAction},
        state::ProcessedActivities,
    },
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const HISTORY_BUFFER: usize = 32;

#[async_trait::async_trait]
trait OutcomePublisher: Sync {
    fn author(&self) -> PublicKey;

    async fn validate(&self, subject: Subject, reply: &agent::Reply) -> Result<(), Error>;

    async fn publish(
        &self,
        subject: Subject,
        delivery: agent::DeliveryId,
        reply: agent::Reply,
    ) -> Result<(), PublishFailure>;

    async fn recover(
        &self,
        subject: Subject,
        delivery: agent::DeliveryId,
        author: PublicKey,
        reply: &agent::Reply,
    ) -> Result<bool, PublishFailure>;
}

struct PublishFailure {
    message: String,
    invalid_target: bool,
}

impl PublishFailure {
    fn into_error(self) -> Error {
        Error::Publish(self.message)
    }
}

#[async_trait::async_trait]
impl<A: agent::Announcer, V: agent::Authorizer> OutcomePublisher for Publisher<A, V> {
    fn author(&self) -> PublicKey {
        Publisher::author(self)
    }

    async fn validate(&self, subject: Subject, reply: &agent::Reply) -> Result<(), Error> {
        Publisher::validate(self, subject, reply)
            .await
            .map_err(|error| Error::Publish(error.to_string()))
    }

    async fn publish(
        &self,
        subject: Subject,
        delivery: agent::DeliveryId,
        reply: agent::Reply,
    ) -> Result<(), PublishFailure> {
        let _operation = Publisher::publish(self, subject, delivery, reply)
            .await
            .map_err(|error| PublishFailure {
                invalid_target: error.is_invalid_target(),
                message: error.to_string(),
            })?;
        Ok(())
    }

    async fn recover(
        &self,
        subject: Subject,
        delivery: agent::DeliveryId,
        author: PublicKey,
        reply: &agent::Reply,
    ) -> Result<bool, PublishFailure> {
        Publisher::recover(self, subject, delivery, author, reply)
            .await
            .map_err(|error| PublishFailure {
                invalid_target: error.is_invalid_target(),
                message: error.to_string(),
            })
    }
}

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

#[derive(Clone, Copy)]
struct HistorySource {
    member_id: NodeId,
    subject: fn(ObjectId) -> Subject,
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
        signer: Option<MemorySigner>,
        authorizer: impl agent::Authorizer,
    ) -> Result<(), Error> {
        let member_id = Self::verify_profile(*profile.id(), node.clone()).await?;
        let storage = profile.storage.clone();
        let state = ProcessedActivities::open(state_path).await?;
        let events = self.subscribe(node.clone()).await?;
        let publisher = Publisher::new(
            profile.clone(),
            self.repository,
            signer,
            node.clone(),
            authorizer,
        );
        self.watch(&storage, member_id, &state, events, agent, &publisher)
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
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        self.scan_all(storage, member_id, state, agent, publisher)
            .await?;
        loop {
            Self::await_update(&mut events).await?;
            self.scan_all(storage, member_id, state, agent, publisher)
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
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        let issues = self.issue_histories(storage.clone());
        self.scan_objects(issues, member_id, state, agent, publisher, Subject::Issue)
            .await?;
        let patches = self.patch_histories(storage.clone());
        self.scan_objects(patches, member_id, state, agent, publisher, Subject::Patch)
            .await
    }

    async fn scan_objects<A>(
        &self,
        mut histories: tokio::sync::mpsc::Receiver<HistoryEvent<A>>,
        member_id: NodeId,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        publisher: &(impl OutcomePublisher + ?Sized),
        subject: fn(ObjectId) -> Subject,
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        let mut active = None;
        let source = HistorySource { member_id, subject };
        while let Some(event) = histories.recv().await {
            self.handle_history_event(event, source, &mut active, state, agent, publisher)
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
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        match event {
            HistoryEvent::Begin { object, actionable } => {
                self.begin_history(object, actionable, source.member_id, source.subject, active)
            }
            HistoryEvent::Operation(operation) => Self::scan_active(operation, active, state).await,
            HistoryEvent::End => Self::finish_active(active, state, agent, publisher).await,
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
        publisher: &(impl OutcomePublisher + ?Sized),
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
                publisher,
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
        publisher: &(impl OutcomePublisher + ?Sized),
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
            .dispatch(
                context.assigned(),
                scope.actionable,
                scope,
                state,
                agent,
                publisher,
            )
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
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        for activity in self.skipped {
            Self::recover_or_checkpoint(activity, scope, state, publisher).await?;
        }
        if let Some(activity) = self.assignment {
            if assigned && actionable {
                Self::deliver(activity, scope, state, agent, publisher).await?;
            } else {
                Self::recover_or_checkpoint(activity, scope, state, publisher).await?;
            }
        }
        for activity in self.activities {
            Self::deliver(activity, scope, state, agent, publisher).await?;
        }
        Ok(())
    }

    async fn deliver(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        agent: &mut (impl Agent + ?Sized),
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        let delivery = activity.delivery_id();
        let subject = activity.subject;
        if Self::publish_pending(&activity, scope, state, publisher).await? {
            return Ok(());
        }
        if let Outcome::Reply(reply) = activity.dispatch(agent).await? {
            publisher.validate(subject, &reply).await?;
            state
                .store_reply(
                    scope.repository,
                    scope.object,
                    delivery.operation,
                    delivery.action,
                    &reply,
                    publisher.author(),
                )
                .await?;
            Self::publish_reply(subject, delivery, reply, scope, state, publisher).await?;
        }
        Self::checkpoint_delivery(delivery, scope, state).await
    }

    async fn recover_or_checkpoint(
        activity: Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        if Self::publish_pending(&activity, scope, state, publisher).await? {
            Ok(())
        } else {
            Self::checkpoint(activity, scope, state).await
        }
    }

    async fn publish_pending(
        activity: &Activity,
        scope: ScanScope,
        state: &ProcessedActivities,
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<bool, Error> {
        let delivery = activity.delivery_id();
        let Some(pending) = state
            .pending_reply(
                scope.repository,
                scope.object,
                delivery.operation,
                delivery.action,
            )
            .await?
        else {
            return Ok(false);
        };
        let author = publisher.author();
        if pending.author != author {
            if publisher
                .recover(activity.subject, delivery, pending.author, &pending.reply)
                .await
                .map_err(PublishFailure::into_error)?
            {
                Self::checkpoint_delivery(delivery, scope, state).await?;
                return Ok(true);
            }
            state
                .store_reply(
                    scope.repository,
                    scope.object,
                    delivery.operation,
                    delivery.action,
                    &pending.reply,
                    author,
                )
                .await?;
        }
        Self::publish_reply(
            activity.subject,
            delivery,
            pending.reply,
            scope,
            state,
            publisher,
        )
        .await?;
        Self::checkpoint_delivery(delivery, scope, state).await?;
        Ok(true)
    }

    async fn publish_reply(
        subject: Subject,
        delivery: agent::DeliveryId,
        reply: agent::Reply,
        scope: ScanScope,
        state: &ProcessedActivities,
        publisher: &(impl OutcomePublisher + ?Sized),
    ) -> Result<(), Error> {
        match publisher.publish(subject, delivery, reply).await {
            Ok(()) => Ok(()),
            Err(failure) => {
                if failure.invalid_target {
                    state
                        .clear_reply(
                            scope.repository,
                            scope.object,
                            delivery.operation,
                            delivery.action,
                        )
                        .await?;
                }
                Err(failure.into_error())
            }
        }
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
    /// Publishing an agent reply failed.
    #[error("failed to publish agent reply: {0}")]
    Publish(String),
    /// Preparing the local watcher database failed.
    #[error("watcher state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Persisted reply recovery state was invalid.
    #[error("invalid pending reply state: {0}")]
    ReplyState(String),
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
    use std::{
        fs, io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
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
    use radicle::{
        cob::{issue::Issues, store::access::ReadOnly},
        crypto::Seed,
        identity::doc::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };
    #[cfg(not(miri))]
    use rmcp::{
        ServiceExt as _, model::CallToolRequestParams, transport::StreamableHttpClientTransport,
    };

    #[cfg(not(miri))]
    use crate::{
        agent::{
            self, Agent, Context as AgentContext, DeliveryId, McpEndpoint, Outcome, Reply,
            ReplyTarget, Target,
        },
        context,
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
    struct NoopPublisher;

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl super::OutcomePublisher for NoopPublisher {
        fn author(&self) -> PublicKey {
            PublicKey::from_str(MEMBER).expect("member key should be valid")
        }

        async fn validate(
            &self,
            _subject: Subject,
            _reply: &crate::agent::Reply,
        ) -> Result<(), super::Error> {
            Err(super::Error::HistoryProtocol(
                "test agent unexpectedly requested a reply",
            ))
        }

        async fn publish(
            &self,
            _subject: Subject,
            _delivery: crate::agent::DeliveryId,
            _reply: crate::agent::Reply,
        ) -> Result<(), super::PublishFailure> {
            Err(super::PublishFailure {
                message: "test agent unexpectedly requested a reply".to_owned(),
                invalid_target: false,
            })
        }

        async fn recover(
            &self,
            _subject: Subject,
            _delivery: crate::agent::DeliveryId,
            _author: PublicKey,
            _reply: &crate::agent::Reply,
        ) -> Result<bool, super::PublishFailure> {
            Err(super::PublishFailure {
                message: "test publisher unexpectedly recovered a reply".to_owned(),
                invalid_target: false,
            })
        }
    }

    #[cfg(not(miri))]
    struct RejectingAgent {
        mentions: usize,
        reject: usize,
        deliveries: Vec<DeliveryId>,
    }

    #[cfg(not(miri))]
    struct ContextReplyAgent {
        endpoint: Option<String>,
        fetched: bool,
    }

    #[cfg(not(miri))]
    struct FixedReplyAgent {
        reply: Reply,
        calls: usize,
    }

    #[cfg(not(miri))]
    #[derive(Debug, Clone)]
    struct RecordingAnnouncer(Arc<AtomicUsize>);

    #[cfg(not(miri))]
    #[derive(Debug, Clone)]
    struct FailOnceAnnouncer(Arc<AtomicUsize>);

    #[cfg(not(miri))]
    #[derive(Debug, Clone, Copy)]
    struct AllowWrites;

    #[cfg(not(miri))]
    #[derive(Debug, Clone, Copy)]
    struct DenyWrites;

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl crate::agent::Authorizer for AllowWrites {
        async fn authorize(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl crate::agent::Authorizer for DenyWrites {
        async fn authorize(&self) -> Result<(), String> {
            Err("binding revoked".to_owned())
        }
    }

    #[cfg(not(miri))]
    impl crate::agent::Announcer for RecordingAnnouncer {
        fn announce(
            &mut self,
            _repository: RepoId,
            _namespace: PublicKey,
        ) -> Result<(), crate::agent::PublishError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[cfg(not(miri))]
    impl crate::agent::Announcer for FailOnceAnnouncer {
        fn announce(
            &mut self,
            _repository: RepoId,
            _namespace: PublicKey,
        ) -> Result<(), crate::agent::PublishError> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(crate::agent::PublishError::Task(
                    "temporary announcement failure".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[cfg(not(miri))]
    impl ContextReplyAgent {
        fn error(error: impl std::fmt::Display) -> agent::Error {
            agent::Error::new(io::Error::other(error.to_string()))
        }
    }

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl Agent for ContextReplyAgent {
        async fn start(&mut self, endpoint: &McpEndpoint) -> Result<(), agent::Error> {
            self.endpoint = Some(endpoint.url().to_string());
            Ok(())
        }

        async fn assigned(&mut self, _context: AgentContext) -> Result<Outcome, agent::Error> {
            Ok(Outcome::NoAction)
        }

        async fn mentioned(
            &mut self,
            context: AgentContext,
            _target: Target,
        ) -> Result<Outcome, agent::Error> {
            let endpoint = self
                .endpoint
                .as_deref()
                .ok_or_else(|| Self::error("agent was not started"))?;
            let client =
                ().serve(StreamableHttpClientTransport::from_uri(endpoint))
                    .await
                    .map_err(Self::error)?;
            let Subject::Issue(issue) = context.subject else {
                return Err(Self::error("expected an issue mention"));
            };
            let arguments = serde_json::json!({
                "kind": "issue",
                "subject": issue.to_string(),
                "thread": "issue"
            })
            .as_object()
            .cloned()
            .ok_or_else(|| Self::error("context arguments should be an object"))?;
            let result = client
                .call_tool(CallToolRequestParams::new("get_thread").with_arguments(arguments))
                .await
                .map_err(Self::error)?;
            self.fetched = !result.content.is_empty() && result.is_error != Some(true);
            let _quit = client.cancel().await.map_err(Self::error)?;

            Ok(Outcome::Reply(Reply {
                body: "I read the discussion.".to_owned(),
                target: ReplyTarget::Issue { reply_to: None },
            }))
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

    #[cfg(not(miri))]
    #[async_trait::async_trait]
    impl Agent for FixedReplyAgent {
        async fn assigned(&mut self, _context: AgentContext) -> Result<Outcome, agent::Error> {
            Ok(Outcome::NoAction)
        }

        async fn mentioned(
            &mut self,
            _context: AgentContext,
            _target: Target,
        ) -> Result<Outcome, agent::Error> {
            self.calls += 1;
            Ok(Outcome::Reply(self.reply.clone()))
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
        #[cfg(not(miri))]
        fn profile_repository() -> (tempfile::TempDir, Profile, RepoId, git2::Oid) {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let home = Home::new(root.path().join("home")).expect("home should be created");
            let profile = Profile::init(home, Alias::new("agent-test"), None, Seed::generate())
                .expect("profile should be initialized");
            let working = root.path().join("working");
            let checkout =
                git2::Repository::init(&working).expect("checkout should be initialized");
            fs::write(working.join("README.md"), "# Test\n").expect("README should be written");
            let mut index = checkout.index().expect("index should open");
            index
                .add_path(std::path::Path::new("README.md"))
                .expect("README should be added");
            let tree = index.write_tree().expect("tree should be written");
            let tree = checkout.find_tree(tree).expect("tree should be found");
            let signature = git2::Signature::now("Agent Test", "agent@example.com")
                .expect("signature should be valid");
            let head = checkout
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "Initial commit",
                    &tree,
                    &[],
                )
                .expect("initial commit should be created");
            checkout
                .set_head("refs/heads/main")
                .expect("HEAD should be set");
            drop(tree);

            let signer = profile.signer().expect("signer should load");
            let (repository, _document, _refs) = radicle::rad::init(
                &checkout,
                "agent-context"
                    .try_into()
                    .expect("project name should be valid"),
                "Agent context test",
                "main".try_into().expect("branch should be valid"),
                Visibility::default(),
                &signer,
                &profile.storage,
            )
            .expect("Radicle repository should be initialized");

            (root, profile, repository, head)
        }

        #[cfg(not(miri))]
        fn profile_with_issue() -> (tempfile::TempDir, Profile, RepoId, ObjectId) {
            let (root, profile, repository, _head) = Self::profile_repository();
            let signer = profile.signer().expect("signer should load");
            let issue = {
                let repository = profile
                    .storage
                    .repository(repository)
                    .expect("stored repository should open");
                let mut issues = profile
                    .issues_mut(&repository, &signer)
                    .expect("issue cache should open");
                let issue = issues
                    .create(
                        Title::try_from("Question".to_owned()).expect("title should be valid"),
                        "@nico, please read this discussion.",
                        &[],
                        &[],
                        [],
                    )
                    .expect("issue should be created");
                *issue.id()
            };

            (root, profile, repository, issue)
        }

        #[cfg(not(miri))]
        async fn assert_rotation_recovery(
            pending_body: &str,
            committed_body: &str,
            unsigned: bool,
            expected_bodies: &[&str],
        ) {
            let (root, profile, repository, issue_id) = Self::profile_with_issue();
            let activity = Activity::new(
                repository,
                Subject::Issue(issue_id),
                *issue_id,
                0,
                profile.did(),
                Kind::Mentioned(Target::Subject),
            );
            let pending = Reply {
                body: pending_body.to_owned(),
                target: ReplyTarget::Issue { reply_to: None },
            };
            let state = ProcessedActivities::memory()
                .await
                .expect("state should open");
            state
                .store_reply(
                    repository,
                    issue_id,
                    activity.operation,
                    activity.action,
                    &pending,
                    *profile.id(),
                )
                .await
                .expect("pending reply should be stored");
            let publisher = crate::agent::Publisher::new(
                profile.clone(),
                repository,
                None,
                FailOnceAnnouncer(Arc::new(AtomicUsize::new(0))),
                AllowWrites,
            );
            let first = publisher
                .publish(
                    activity.subject,
                    activity.delivery_id(),
                    Reply {
                        body: committed_body.to_owned(),
                        target: ReplyTarget::Issue { reply_to: None },
                    },
                )
                .await;
            assert!(matches!(first, Err(crate::agent::PublishError::Task(_))));
            if unsigned {
                let repository = profile
                    .storage
                    .repository(repository)
                    .expect("stored repository should open");
                let sigrefs = format!("refs/namespaces/{}/refs/rad/sigrefs", profile.id());
                repository
                    .backend
                    .find_reference(&sigrefs)
                    .expect("signed refs should exist")
                    .delete()
                    .expect("signed refs should be removed");
            }

            let rotated_home =
                Home::new(root.path().join("rotated")).expect("rotated home should be created");
            let mut rotated = Profile::init(
                rotated_home,
                Alias::new("rotated-agent"),
                None,
                Seed::generate(),
            )
            .expect("rotated profile should be initialized");
            rotated.storage = profile.storage.clone();
            let scope = super::ScanScope::new(
                Member::Nico,
                *rotated.id(),
                repository,
                issue_id,
                Subject::Issue(issue_id),
                true,
            );
            let announcements = Arc::new(AtomicUsize::new(0));
            let rotated_publisher = crate::agent::Publisher::new(
                rotated,
                repository,
                None,
                RecordingAnnouncer(announcements.clone()),
                AllowWrites,
            );
            let mut agent = RejectingAgent {
                mentions: 0,
                reject: 1,
                deliveries: Vec::new(),
            };
            super::PendingActivities::deliver(
                activity.clone(),
                scope,
                &state,
                &mut agent,
                &rotated_publisher,
            )
            .await
            .expect("pending reply should recover after rotation");

            assert_eq!(agent.mentions, 0);
            assert_eq!(announcements.load(Ordering::Relaxed), 1);
            assert!(
                state
                    .contains(repository, issue_id, activity.operation, activity.action)
                    .await
                    .expect("processed state should load")
            );
            let repository = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let issue = Issues::open(&repository, ReadOnly)
                .expect("issues should open")
                .get(&issue_id)
                .expect("issue should load")
                .expect("issue should exist");
            let mut bodies = issue
                .replies()
                .map(|(_id, reply)| reply.body().to_owned())
                .collect::<Vec<_>>();
            bodies.sort();
            let mut expected = expected_bodies
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(bodies, expected);
        }

        #[cfg(not(miri))]
        fn profile_with_redacted_issue_comment()
        -> (tempfile::TempDir, Profile, RepoId, ObjectId, EntryId) {
            let (root, profile, repository, issue_id) = Self::profile_with_issue();
            let signer = profile.signer().expect("signer should load");
            let comment = {
                let repository = profile
                    .storage
                    .repository(repository)
                    .expect("stored repository should open");
                let mut issues = profile
                    .issues_mut(&repository, &signer)
                    .expect("issue cache should open");
                let mut issue = issues.get_mut(&issue_id).expect("issue should load");
                let comment = issue
                    .comment("Temporary comment", *issue_id, [])
                    .expect("comment should be created");
                issue
                    .redact_comment(comment)
                    .expect("comment should be redacted");
                comment
            };

            (root, profile, repository, issue_id, comment)
        }

        #[cfg(not(miri))]
        fn profile_with_redacted_patch_discussions() -> (
            tempfile::TempDir,
            Profile,
            RepoId,
            ObjectId,
            RevisionId,
            ReviewId,
        ) {
            Self::profile_with_patch_discussions(true)
        }

        #[cfg(not(miri))]
        fn profile_with_patch_discussions(
            redact: bool,
        ) -> (
            tempfile::TempDir,
            Profile,
            RepoId,
            ObjectId,
            RevisionId,
            ReviewId,
        ) {
            let (root, profile, repository, head) = Self::profile_repository();
            let signer = profile.signer().expect("signer should load");
            let (patch_id, revision, review) = {
                let repository = profile
                    .storage
                    .repository(repository)
                    .expect("stored repository should open");
                let mut patches = profile
                    .patches_mut(&repository, &signer)
                    .expect("patch cache should open");
                let mut patch = patches
                    .create(
                        Title::try_from("Patch".to_owned()).expect("title should be valid"),
                        "Description",
                        MergeTarget::default(),
                        head,
                        head,
                        &[],
                    )
                    .expect("patch should be created");
                let patch_id = *patch.id();
                let (root_revision, _value) = patch.root();
                let review = patch
                    .review(root_revision, None, Some("Review".to_owned()), Vec::new())
                    .expect("review should be created");
                let revision = if redact {
                    patch
                        .redact_review(review)
                        .expect("review should be redacted");
                    let revision = patch
                        .update("Second revision", head, head)
                        .expect("revision should be created");
                    patch.redact(revision).expect("revision should be redacted");
                    revision
                } else {
                    root_revision
                };
                (patch_id, revision, review)
            };

            (root, profile, repository, patch_id, revision, review)
        }

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
                &NoopPublisher,
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
                &NoopPublisher,
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

        super::Watcher::scan_history(
            std::iter::once(Ok(operation)),
            scope,
            &state,
            &mut agent,
            &NoopPublisher,
        )
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

    #[tokio::test]
    #[cfg(not(miri))]
    async fn agent_fetches_mcp_context_before_publishing_an_issue_reply() {
        let (_root, profile, repository, issue_id) = Fixture::profile_with_issue();
        let server = context::Server::start(profile.storage.clone(), repository)
            .await
            .expect("context server should start");
        let mut agent = ContextReplyAgent {
            endpoint: None,
            fetched: false,
        };
        agent
            .start(server.endpoint())
            .await
            .expect("agent should start");
        let activity = Activity::new(
            repository,
            Subject::Issue(issue_id),
            *issue_id,
            0,
            profile.did(),
            Kind::Mentioned(Target::Subject),
        );
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        let scope = super::ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue_id,
            Subject::Issue(issue_id),
            true,
        );

        super::PendingActivities::deliver(activity.clone(), scope, &state, &mut agent, &publisher)
            .await
            .expect("agent reply should be published");
        super::PendingActivities::deliver(activity, scope, &state, &mut agent, &publisher)
            .await
            .expect("retried agent reply should be deduplicated");

        assert!(agent.fetched);
        assert_eq!(announcements.load(Ordering::Relaxed), 2);
        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        let issue = Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .get(&issue_id)
            .expect("issue should load")
            .expect("issue should exist");
        let replies = issue.replies().collect::<Vec<_>>();
        assert_eq!(replies.len(), 1);
        let (_id, reply) = replies.first().expect("reply should exist");
        assert_eq!(reply.body(), "I read the discussion.");
        assert_eq!(reply.reply_to(), Some(*issue_id));

        server.stop().await.expect("context server should stop");
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn recovers_a_pending_committed_reply_without_redispatch() {
        let (_root, profile, repository, issue_id) = Fixture::profile_with_issue();
        let activity = Activity::new(
            repository,
            Subject::Issue(issue_id),
            *issue_id,
            0,
            profile.did(),
            Kind::Mentioned(Target::Subject),
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            FailOnceAnnouncer(attempts.clone()),
            AllowWrites,
        );
        let reply = Reply {
            body: "Committed before announcement failed.".to_owned(),
            target: ReplyTarget::Issue { reply_to: None },
        };
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        let scope = super::ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue_id,
            Subject::Issue(issue_id),
            true,
        );
        state
            .store_reply(
                repository,
                issue_id,
                activity.operation,
                activity.action,
                &reply,
                *profile.id(),
            )
            .await
            .expect("pending reply should be stored");
        let first = publisher
            .publish(activity.subject, activity.delivery_id(), reply)
            .await;
        assert!(matches!(first, Err(crate::agent::PublishError::Task(_))));

        let mut agent = RejectingAgent {
            mentions: 0,
            reject: 1,
            deliveries: Vec::new(),
        };
        super::PendingActivities::deliver(activity, scope, &state, &mut agent, &publisher)
            .await
            .expect("committed reply should be announced");

        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert_eq!(agent.mentions, 0);
        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        let issue = Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .get(&issue_id)
            .expect("issue should load")
            .expect("issue should exist");
        assert_eq!(issue.replies().count(), 1);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn recovers_a_committed_reply_after_signing_key_rotation() {
        Fixture::assert_rotation_recovery(
            "Committed with the previous key.",
            "Committed with the previous key.",
            false,
            &["Committed with the previous key."],
        )
        .await;
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn rejects_a_previous_key_marker_with_different_reply_content() {
        Fixture::assert_rotation_recovery(
            "Intended reply.",
            "Injected reply.",
            false,
            &["Intended reply.", "Injected reply."],
        )
        .await;
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn replaces_an_unsigned_previous_key_reply_after_rotation() {
        Fixture::assert_rotation_recovery(
            "Reply after an interrupted signing step.",
            "Reply after an interrupted signing step.",
            true,
            &[
                "Reply after an interrupted signing step.",
                "Reply after an interrupted signing step.",
            ],
        )
        .await;
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn invalid_replies_do_not_wedge_retry_state() {
        let (_root, profile, repository, issue) = Fixture::profile_with_issue();
        let activity = Activity::new(
            repository,
            Subject::Issue(issue),
            *issue,
            0,
            profile.did(),
            Kind::Mentioned(Target::Subject),
        );
        let scope = super::ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue,
            Subject::Issue(issue),
            true,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            RecordingAnnouncer(Arc::new(AtomicUsize::new(0))),
            AllowWrites,
        );
        let mut agent = FixedReplyAgent {
            reply: Reply {
                body: "Invalid parent".to_owned(),
                target: ReplyTarget::Issue {
                    reply_to: Some(EntryId::from_str(COMMENT).expect("comment should be valid")),
                },
            },
            calls: 0,
        };

        let first = super::PendingActivities::deliver(
            activity.clone(),
            scope,
            &state,
            &mut agent,
            &publisher,
        )
        .await;
        assert!(matches!(first, Err(super::Error::Publish(_))));
        assert_eq!(
            state
                .pending_reply(repository, issue, activity.operation, activity.action)
                .await
                .expect("pending state should load"),
            None
        );

        agent.reply = Reply {
            body: "Corrected reply".to_owned(),
            target: ReplyTarget::Issue { reply_to: None },
        };
        super::PendingActivities::deliver(activity, scope, &state, &mut agent, &publisher)
            .await
            .expect("corrected reply should publish");

        assert_eq!(agent.calls, 2);
        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        let issue = Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .get(&issue)
            .expect("issue should load")
            .expect("issue should exist");
        assert_eq!(issue.replies().count(), 1);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn recovers_pending_assignment_replies_after_assignment_changes() {
        let (_root, profile, repository, issue) = Fixture::profile_with_issue();
        let activity = Activity::new(
            repository,
            Subject::Issue(issue),
            *issue,
            0,
            profile.did(),
            Kind::Assigned,
        );
        let scope = super::ScanScope::new(
            Member::Nico,
            *profile.id(),
            repository,
            issue,
            Subject::Issue(issue),
            false,
        );
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        state
            .store_reply(
                repository,
                issue,
                activity.operation,
                activity.action,
                &Reply {
                    body: "Pending assignment reply".to_owned(),
                    target: ReplyTarget::Issue { reply_to: None },
                },
                *profile.id(),
            )
            .await
            .expect("pending reply should be stored");
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );
        let mut agent = RejectingAgent {
            mentions: 0,
            reject: 1,
            deliveries: Vec::new(),
        };
        let pending = super::PendingActivities {
            assignment: Some(activity.clone()),
            ..super::PendingActivities::default()
        };

        pending
            .dispatch(false, false, scope, &state, &mut agent, &publisher)
            .await
            .expect("pending assignment reply should recover");

        assert_eq!(announcements.load(Ordering::Relaxed), 1);
        assert!(
            state
                .contains(repository, issue, activity.operation, activity.action)
                .await
                .expect("processed state should load")
        );
        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        let issue = Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .get(&issue)
            .expect("issue should load")
            .expect("issue should exist");
        assert_eq!(issue.replies().count(), 1);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn publishes_without_an_issue_cache_and_resigns_existing_refs() {
        let (_root, profile, repository, issue) = Fixture::profile_with_issue();
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );
        let delivery = DeliveryId {
            operation: *issue,
            action: 0,
        };
        let reply = Reply {
            body: "Reply".to_owned(),
            target: ReplyTarget::Issue { reply_to: None },
        };
        let mut cache = profile.cobs_db_mut().expect("COB cache should be writable");
        assert!(
            radicle::cob::cache::Remove::<radicle::cob::issue::Issue>::remove(&mut cache, &issue)
                .expect("issue cache removal should succeed")
        );
        drop(cache);
        publisher
            .publish(Subject::Issue(issue), delivery, reply.clone())
            .await
            .expect("reply should publish");
        {
            let repository = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let sigrefs = format!("refs/namespaces/{}/refs/rad/sigrefs", profile.id());
            repository
                .backend
                .find_reference(&sigrefs)
                .expect("signed refs should exist")
                .delete()
                .expect("signed refs should be removed");
        }

        publisher
            .publish(Subject::Issue(issue), delivery, reply)
            .await
            .expect("existing reply should repair signed refs");

        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        assert!(
            radicle::storage::refs::SignedRefs::load(*profile.id(), &repository)
                .expect("signed refs should load")
                .is_some()
        );
        assert_eq!(announcements.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn publishes_revision_and_review_replies_without_a_patch_cache() {
        let (_root, profile, repository, patch, revision, review) =
            Fixture::profile_with_patch_discussions(false);
        let mut cache = profile.cobs_db_mut().expect("COB cache should be writable");
        assert!(
            radicle::cob::cache::Remove::<radicle::cob::patch::Patch>::remove(&mut cache, &patch)
                .expect("patch cache removal should succeed")
        );
        drop(cache);
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile,
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );

        publisher
            .publish(
                Subject::Patch(patch),
                DeliveryId {
                    operation: *patch,
                    action: 0,
                },
                Reply {
                    body: "Revision reply".to_owned(),
                    target: ReplyTarget::Revision {
                        revision,
                        reply_to: None,
                    },
                },
            )
            .await
            .expect("revision reply should publish");
        publisher
            .publish(
                Subject::Patch(patch),
                DeliveryId {
                    operation: *patch,
                    action: 1,
                },
                Reply {
                    body: "Review reply".to_owned(),
                    target: ReplyTarget::Review {
                        review,
                        reply_to: None,
                    },
                },
            )
            .await
            .expect("review reply should publish");

        assert_eq!(announcements.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn rejects_replies_to_redacted_patch_discussions() {
        let (_root, profile, repository, patch, revision, review) =
            Fixture::profile_with_redacted_patch_discussions();
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile,
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );
        let revision_result = publisher
            .publish(
                Subject::Patch(patch),
                DeliveryId {
                    operation: *patch,
                    action: 0,
                },
                Reply {
                    body: "This must not disappear.".to_owned(),
                    target: ReplyTarget::Revision {
                        revision,
                        reply_to: None,
                    },
                },
            )
            .await;
        let review_result = publisher
            .publish(
                Subject::Patch(patch),
                DeliveryId {
                    operation: *patch,
                    action: 1,
                },
                Reply {
                    body: "This must not disappear either.".to_owned(),
                    target: ReplyTarget::Review {
                        review,
                        reply_to: None,
                    },
                },
            )
            .await;

        assert!(matches!(
            revision_result,
            Err(crate::agent::PublishError::PatchTargetUnavailable { .. })
        ));
        assert!(matches!(
            review_result,
            Err(crate::agent::PublishError::PatchTargetUnavailable { .. })
        ));
        assert_eq!(announcements.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn rejects_a_reply_to_a_redacted_issue_parent() {
        let (_root, profile, repository, issue, parent) =
            Fixture::profile_with_redacted_issue_comment();
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile,
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            AllowWrites,
        );

        let result = publisher
            .publish(
                Subject::Issue(issue),
                DeliveryId {
                    operation: *issue,
                    action: 0,
                },
                Reply {
                    body: "Reply".to_owned(),
                    target: ReplyTarget::Issue {
                        reply_to: Some(parent),
                    },
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::agent::PublishError::IssueParentUnavailable { .. })
        ));
        assert_eq!(announcements.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn rejects_publication_after_authorization_is_revoked() {
        let (_root, profile, repository, issue) = Fixture::profile_with_issue();
        let announcements = Arc::new(AtomicUsize::new(0));
        let publisher = crate::agent::Publisher::new(
            profile.clone(),
            repository,
            None,
            RecordingAnnouncer(announcements.clone()),
            DenyWrites,
        );

        let result = publisher
            .publish(
                Subject::Issue(issue),
                DeliveryId {
                    operation: *issue,
                    action: 0,
                },
                Reply {
                    body: "Unauthorized reply".to_owned(),
                    target: ReplyTarget::Issue { reply_to: None },
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::agent::PublishError::Authorization(_))
        ));
        assert_eq!(announcements.load(Ordering::Relaxed), 0);
        let repository = profile
            .storage
            .repository(repository)
            .expect("stored repository should open");
        let issue = Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .get(&issue)
            .expect("issue should load")
            .expect("issue should exist");
        assert_eq!(issue.replies().count(), 0);
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
