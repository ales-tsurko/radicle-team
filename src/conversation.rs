//! Shared conversation policy and serialized delivery to the talking half of a member.

use std::collections::BTreeMap;

use radicle::identity::Did;
use tokio::sync::{mpsc, oneshot};

use crate::{
    agent::{self, Talker, WorkId},
    manifest::{Limits, TriggerAccess},
    mcp::WorkScope,
    member::Member,
    watcher::{Activity, Delivery},
    workspace::Workspaces,
};

const BUFFER: usize = 32;

/// Repository trigger policy resolved against the current delegate set.
#[derive(Debug, Clone)]
pub(crate) struct Triggers {
    access: TriggerAccess,
    delegates: Vec<Did>,
    members: Vec<Did>,
}

impl Triggers {
    /// Creates a trigger classifier from validated repository policy.
    pub(crate) const fn new(access: TriggerAccess, delegates: Vec<Did>, members: Vec<Did>) -> Self {
        Self {
            access,
            delegates,
            members,
        }
    }

    /// Returns who `author` is to the member handling the conversation.
    pub(crate) fn author(&self, author: Did, member: Did) -> Author {
        if author == member {
            Author::Member
        } else if self.members.contains(&author) {
            Author::Peer
        } else if self.access.allows(author, &self.delegates) {
            Author::Person
        } else {
            Author::Stranger
        }
    }
}

/// Who an author is to the member handling a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Author {
    /// The member itself.
    Member,
    /// Another member of the same team.
    Peer,
    /// Somebody outside the team who may put its members to work.
    Person,
    /// Somebody the repository does not let put members to work.
    Stranger,
}

impl Author {
    /// Checks the addressing form allowed for this kind of author.
    pub(crate) fn addresses(self, member: Member, body: &str) -> bool {
        if self == Self::Peer {
            member.is_named_in(body)
        } else {
            member.is_mentioned_in(body)
        }
    }
}

/// How far one conversation has run since a person last joined it.
#[derive(Debug, Default)]
pub(crate) struct Turn {
    charged: Option<Did>,
    exchanges: BTreeMap<Did, usize>,
    limits: Limits,
}

impl Turn {
    /// Starts a conversation under the repository's limits.
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Ends the turn in progress when the next message has another author.
    pub(crate) fn begin(&mut self, author: Did) {
        if self.charged != Some(author) {
            self.charged = None;
        }
    }

    /// Charges an addressed author for waking the member.
    pub(crate) fn admit(&mut self, author: Did, origin: Author) -> Result<(), AdmissionError> {
        match origin {
            Author::Stranger => return Err(AdmissionError::Unauthorized),
            Author::Member | Author::Person => {
                self.exchanges.clear();
                return Ok(());
            }
            Author::Peer => {}
        }
        let spent = self.exchanges.entry(author).or_default();
        if self.charged != Some(author) {
            self.charged = Some(author);
            *spent += 1;
        }
        if *spent <= self.limits.exchanges() {
            Ok(())
        } else {
            Err(AdmissionError::Limit)
        }
    }
}

/// Reason an addressed author cannot continue a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AdmissionError {
    /// Repository trigger policy does not authorize the author.
    #[error("its author may not trigger artificial contributors")]
    Unauthorized,
    /// The conversation has exhausted its autonomous turn allowance.
    #[error("this conversation has run its allowed number of turns since a person last joined it")]
    Limit,
}

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
