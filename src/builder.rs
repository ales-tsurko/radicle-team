//! Queued work carried out by one member's building half.
//!
//! This is the counterpart of [`Watcher`](crate::watcher::Watcher): where the watcher turns
//! repository activity into conversation, this turns the queue into commits. It runs in the same
//! process and under the same identity, but on its own MCP server, so a long job never delays an
//! answer.
//!
//! One job runs at a time. Cancelling asks the adapter to stop and commit what it has, rather than
//! discarding it, because a request to stop usually means priorities moved. Work is taken in the
//! order the queue hands it out; nothing here decides what matters.

use radicle::identity::RepoId;

use crate::{
    agent::{self, Coder, WorkId},
    mcp::{WorkContext, WorkScope},
    member::Member,
    queue::{Job, WorkQueue},
    workspace::Workspaces,
};

/// Drives one member's building half through its queued work.
#[derive(Debug, Clone)]
pub struct Builder {
    member: Member,
    repository: RepoId,
}

impl Builder {
    /// Creates a builder for `member` and `repository`.
    #[must_use]
    pub const fn new(member: Member, repository: RepoId) -> Self {
        Self { member, repository }
    }

    /// Takes queued work and carries it out until the queue fails or the process ends.
    pub(crate) async fn run(
        &self,
        coder: &(impl Coder + ?Sized),
        queue: &WorkQueue,
        workspaces: &Workspaces,
        scope: &WorkScope,
    ) -> Result<(), Error> {
        loop {
            let job = queue.next().await?;
            let carried = self.carry(coder, queue, workspaces, scope, job).await;
            queue.finish().await;
            carried?;
        }
    }

    async fn carry(
        &self,
        coder: &(impl Coder + ?Sized),
        queue: &WorkQueue,
        workspaces: &Workspaces,
        scope: &WorkScope,
        job: Job,
    ) -> Result<(), Error> {
        let work = WorkId::Job(job.id);
        let subject = job.source;
        let workspace = workspaces
            .prepare(work, subject)
            .await
            .map_err(|error| Error::Workspace(error.to_string()))?;
        let path = workspace.path().to_path_buf();
        scope
            .activate(WorkContext {
                repository: self.repository,
                subject,
                work,
                workspace: path.clone(),
            })
            .await
            .map_err(Error::WorkScope)?;
        tracing::info!(
            member = %self.member,
            repository = %self.repository,
            job = job.id,
            %subject,
            workspace = %path.display(),
            "starting queued work"
        );
        let built = Self::build(coder, queue, job, path).await;
        scope.deactivate().await;
        let reconciled = scope.reconcile(work).await;
        let cleaned = workspace.cleanup().await;
        built?;
        reconciled.map_err(Error::PublicationRecovery)?;
        cleaned.map_err(|error| Error::Workspace(error.to_string()))
    }

    async fn build(
        coder: &(impl Coder + ?Sized),
        queue: &WorkQueue,
        job: Job,
        workspace: std::path::PathBuf,
    ) -> Result<(), Error> {
        let identifier = job.id;
        let mut asked = false;
        let building = coder.work(job, workspace);
        tokio::pin!(building);
        loop {
            tokio::select! {
                built = &mut building => {
                    return built.map_err(Error::from).inspect_err(|error| {
                        tracing::warn!(job = identifier, %error, "queued work failed");
                    });
                }
                () = queue.cancelled(identifier), if !asked => {
                    tracing::info!(job = identifier, "asking the running job to stop");
                    asked = true;
                    coder.interrupt().await?;
                }
            }
        }
    }
}

/// Error carrying out queued work.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading the work queue failed.
    #[error("work queue error: {0}")]
    Queue(#[from] crate::queue::Error),
    /// The building adapter failed.
    #[error(transparent)]
    Coder(#[from] agent::Error),
    /// Preparing or cleaning the workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the work-scoped MCP capability failed.
    #[error("work scope failed: {0}")]
    WorkScope(String),
    /// Recovering durable MCP publications after a job failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{path::PathBuf, str::FromStr as _, sync::Arc};

    use radicle::{cob::ObjectId, identity::RepoId};

    use crate::{
        agent::{Coder, Error as AgentError, McpEndpoint},
        builder::Builder,
        manifest::QueuePosition,
        member::{Member, Profile},
        queue::{Job, WorkQueue},
        watcher::Subject,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";

    /// A builder which finishes only once it has been asked to stop.
    #[derive(Debug, Default)]
    struct InterruptibleCoder {
        stop: Arc<tokio::sync::Notify>,
        interruptions: Arc<tokio::sync::Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl Coder for InterruptibleCoder {
        async fn start(
            &mut self,
            _profile: Profile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
        ) -> Result<(), AgentError> {
            Ok(())
        }

        async fn work(&self, _job: Job, _workspace: PathBuf) -> Result<(), AgentError> {
            self.stop.notified().await;
            Ok(())
        }

        async fn interrupt(&self) -> Result<(), AgentError> {
            *self.interruptions.lock().await += 1;
            self.stop.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn asks_the_running_job_to_stop_without_dropping_it() {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
        let queue = WorkQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let source = Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse"));
        let _id = queue
            .push("write it".to_owned(), source, QueuePosition::End)
            .await
            .expect("job should be recorded");
        let job = queue.next().await.expect("queue should be readable");
        let coder = InterruptibleCoder::default();

        let (built, cancelled) = tokio::join!(
            Builder::build(&coder, &queue, job.clone(), PathBuf::from("/tmp/workspace")),
            queue.cancel(),
        );

        built.expect("an interrupted job reports success once it has preserved its work");
        assert_eq!(cancelled.map(|running| running.id), Some(job.id));
        assert_eq!(
            *coder.interruptions.lock().await,
            1,
            "the adapter must be asked to stop rather than have its work dropped, so that it \
             reaches a point where nothing is lost"
        );
    }
}
