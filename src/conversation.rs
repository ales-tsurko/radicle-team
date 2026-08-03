//! Serialized delivery to the talking half of a member.

use tokio::sync::{mpsc, oneshot};

use crate::{
    agent::{self, Talker, WorkId},
    mcp::WorkScope,
    member::Member,
    watcher::{Activity, Delivery},
    workspace::Workspaces,
};

const BUFFER: usize = 32;

/// Sends conversation work to the single dispatcher which owns the talking adapter.
#[derive(Debug, Clone)]
pub(crate) struct Handle {
    sender: mpsc::Sender<Request>,
}

impl Handle {
    /// Delivers one classified interaction and waits until handling and publication recovery
    /// finish.
    pub(crate) async fn deliver(
        &self,
        activity: Activity,
        delivery: Delivery,
    ) -> Result<Acknowledgement, Error> {
        let (completion, completed) = oneshot::channel();
        self.sender
            .send(Request::Deliver {
                activity,
                delivery,
                completion,
            })
            .await
            .map_err(|_error| Error::Unavailable)?;
        completed.await.map_err(|_error| Error::Unavailable)?
    }

    /// Recovers publications made by a delivery which no longer needs agent attention.
    pub(crate) async fn reconcile(&self, work: WorkId) -> Result<(), Error> {
        let (completion, completed) = oneshot::channel();
        self.sender
            .send(Request::Reconcile { work, completion })
            .await
            .map_err(|_error| Error::Unavailable)?;
        completed.await.map_err(|_error| Error::Unavailable)?
    }
}

/// Completion acknowledged after handling and publication recovery succeed.
#[derive(Debug)]
pub(crate) struct Acknowledgement {
    cleanup: Result<(), String>,
}

impl Acknowledgement {
    /// Reports workspace cleanup after the watcher has durably checkpointed the delivery.
    pub(crate) fn finish(self) -> Result<(), Error> {
        self.cleanup.map_err(Error::Workspace)
    }
}

/// Owns the talking adapter's workspace and MCP scope while serializing deliveries.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    member: Member,
    workspaces: Workspaces,
    scope: WorkScope,
    receiver: mpsc::Receiver<Request>,
}

impl Dispatcher {
    /// Creates the dispatcher and its clonable ingress handle.
    pub(crate) fn new(member: Member, workspaces: Workspaces, scope: WorkScope) -> (Handle, Self) {
        let (sender, receiver) = mpsc::channel(BUFFER);
        (
            Handle { sender },
            Self {
                member,
                workspaces,
                scope,
                receiver,
            },
        )
    }

    /// Runs until every ingress handle has been dropped.
    pub(crate) async fn run(mut self, agent: &mut (impl Talker + ?Sized)) -> Result<(), Error> {
        while let Some(request) = self.receiver.recv().await {
            match request {
                Request::Deliver {
                    activity,
                    delivery,
                    completion,
                } => {
                    let result = self.dispatch(activity, delivery, agent).await;
                    let _delivered = completion.send(result);
                }
                Request::Reconcile { work, completion } => {
                    let result = self.reconcile(work).await;
                    let _reconciled = completion.send(result);
                }
            }
        }
        Err(Error::Unavailable)
    }

    async fn dispatch(
        &self,
        activity: Activity,
        delivery: Delivery,
        agent: &mut (impl Talker + ?Sized),
    ) -> Result<Acknowledgement, Error> {
        let identity = activity.delivery_id();
        let subject = activity.subject;
        let workspace = self
            .workspaces
            .prepare(identity.into(), subject.into())
            .await
            .map_err(|error| Error::Workspace(error.to_string()))?;
        let path = workspace.path().to_path_buf();
        self.scope
            .activate(activity.context(path.clone()))
            .await
            .map_err(Error::WorkScope)?;
        tracing::info!(
            member = %self.member,
            repository = %activity.repository,
            ?subject,
            operation = %identity.operation,
            action = identity.action,
            workspace = %path.display(),
            "dispatching activity to the agent"
        );
        let handled = delivery.dispatch(activity.context(path), agent).await;
        self.scope.deactivate().await;
        let cleanup = workspace.cleanup().await.map_err(|error| error.to_string());
        handled.inspect_err(|error| {
            tracing::warn!(
                member = %self.member,
                operation = %identity.operation,
                action = identity.action,
                %error,
                "agent failed to handle activity"
            );
        })?;
        self.reconcile(identity.into()).await?;

        Ok(Acknowledgement { cleanup })
    }

    async fn reconcile(&self, work: WorkId) -> Result<(), Error> {
        self.scope
            .reconcile(work)
            .await
            .map_err(Error::PublicationRecovery)
    }
}

enum Request {
    Deliver {
        activity: Activity,
        delivery: Delivery,
        completion: oneshot::Sender<Result<Acknowledgement, Error>>,
    },
    Reconcile {
        work: WorkId,
        completion: oneshot::Sender<Result<(), Error>>,
    },
}

/// Failure while serializing or handling a conversation delivery.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The talking adapter failed.
    #[error(transparent)]
    Talker(#[from] agent::Error),
    /// Preparing or cleaning the delivery workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the work-scoped MCP capability failed.
    #[error("work scope failed: {0}")]
    WorkScope(String),
    /// Recovering durable MCP publications failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
    /// The dispatcher is not running.
    #[error("conversation dispatcher is not running")]
    Unavailable,
}
