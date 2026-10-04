//! Durable admission and serialized execution of repository conversations.

use crate::{
    agent::{self, ActivityId, Role, Source, Talker},
    delivery::{Inbox, State},
    mcp::ActivityScope,
    member::Member,
    watcher::{Activity, Delivery},
    workspace::Workspaces,
};

/// Runs one delivery, aborting the agent if it stops making progress.
///
/// A delivery has no time limit, because reading a long patch or a long thread legitimately takes
/// a while. Silence is what is limited, on the terms [`Stall`](agent::Stall) sets.
pub(crate) async fn supervise<T>(
    handling: impl Future<Output = Result<T, agent::Error>>,
    agent: std::sync::Arc<agent::Session>,
) -> Result<T, Error> {
    let mut stall = agent::Stall::new(agent);
    tokio::pin!(handling);
    loop {
        tokio::select! {
            handled = &mut handling => return handled.map_err(Error::Talker),
            silent = stall.silent() => {
                if silent {
                    stall.abort().await.map_err(Error::Talker)?;

                    return Err(Error::Stalled);
                }
            }
        }
    }
}

/// The workspace and MCP scope one role works inside.
///
/// Each role runs its own agent under its own MCP server, so each needs its own checkout and its
/// own active-activity gate. Only the request types differ between them.
#[derive(Debug)]
pub(crate) struct Scoped {
    pub(crate) member: Member,
    role: Role,
    workspaces: Workspaces,
    pub(crate) scope: ActivityScope,
}

impl Scoped {
    pub(crate) const fn new(
        member: Member,
        role: Role,
        workspaces: Workspaces,
        scope: ActivityScope,
    ) -> Self {
        Self {
            member,
            role,
            workspaces,
            scope,
        }
    }

    /// Moves this role's working copy onto `source` and reports its path.
    pub(crate) async fn prepare(&self, source: Source) -> Result<std::path::PathBuf, Error> {
        self.workspaces
            .prepare(self.role, source)
            .await
            .map_err(|error| Error::Workspace(error.to_string()))
    }

    /// Closes the scope and recovers interrupted publications.
    pub(crate) async fn finish(
        &self,
        activity: ActivityId,
        handled: Result<(), Error>,
    ) -> Result<(), Error> {
        self.scope.deactivate().await;
        self.reconcile(activity).await?;
        handled.inspect_err(|error| {
            tracing::warn!(member = %self.member, %activity, %error, "agent failed to handle conversation");
        })
    }

    /// Recovers publications belonging to `activity`.
    pub(crate) async fn reconcile(&self, activity: ActivityId) -> Result<(), Error> {
        self.scope
            .reconcile(activity)
            .await
            .map_err(Error::PublicationRecovery)
    }
}

/// Owns the talking role's workspace and MCP scope while serializing deliveries.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    scoped: Scoped,
    inbox: Inbox,
}

impl Dispatcher {
    /// Creates the single consumer of the durable inbox.
    pub(crate) fn new(
        member: Member,
        workspaces: Workspaces,
        scope: ActivityScope,
        inbox: Inbox,
    ) -> Self {
        Self {
            scoped: Scoped::new(member, Role::Talker, workspaces, scope),
            inbox,
        }
    }

    /// Handles one delivery at a time. Reviews and direct addresses precede ambient changes;
    /// FIFO is preserved within each priority, and active handling is never interrupted.
    pub(crate) async fn run(self, agent: &mut (impl Talker + ?Sized)) -> Result<(), Error> {
        loop {
            let record = self.inbox.next().await?;
            let crate::watcher::Kind::Delivered(delivery) = record.activity.kind else {
                continue;
            };
            let result = self
                .dispatch(record.activity.clone(), delivery, agent)
                .await;
            match result {
                Ok(()) => self.inbox.set(&record.id(), State::Handled, None).await?,
                Err(error @ (Error::Talker(_) | Error::Stalled)) => {
                    self.inbox
                        .set(&record.id(), State::Failed, Some(error.to_string()))
                        .await?;
                    agent::Supervised::supervisor(agent).handled().await;
                    tracing::warn!(%error, "conversation failed; retained for retry");
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn dispatch(
        &self,
        activity: Activity,
        delivery: Delivery,
        agent: &mut (impl Talker + ?Sized),
    ) -> Result<(), Error> {
        let identity = activity.delivery_id();
        let subject = activity.subject;
        let watch = agent::Supervised::supervisor(agent);
        watch.handling(&subject.into(), None).await;
        let path = self.scoped.prepare(subject.into()).await?;
        self.scoped
            .scope
            .activate(activity.context(path.clone()))
            .await
            .map_err(Error::ActivityScope)?;
        tracing::info!(
            member = %self.scoped.member,
            repository = %activity.repository,
            ?subject,
            operation = %identity.operation,
            action = identity.action,
            workspace = %path.display(),
            "dispatching activity to the agent"
        );
        let handled = supervise(
            delivery.dispatch(activity.context(path), agent),
            watch.clone(),
        )
        .await;

        self.scoped.finish(identity.into(), handled).await?;
        watch.handled().await;
        Ok(())
    }
}

/// Failure while serializing or handling a conversation delivery.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Inbox(#[from] crate::delivery::Error),
    /// The talking agent failed.
    #[error(transparent)]
    Talker(#[from] agent::Error),
    /// Preparing or cleaning the delivery workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the activity-scoped MCP capability failed.
    #[error("activity scope failed: {0}")]
    ActivityScope(String),
    /// Recovering durable MCP publications failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
    /// The dispatcher is not running.
    #[error("conversation dispatcher is not running")]
    Unavailable,
    /// The agent stopped making progress and was aborted.
    #[error("agent was stopped after making no progress")]
    Stalled,
}
