//! Queued work carried out by one member's building role.
//!
//! This is the counterpart of [`Watcher`](crate::watcher::Watcher): where the watcher turns
//! repository activity into conversation, this turns the queue into commits. It runs in the same
//! process and under the same identity, but on its own MCP server, so a long task never delays an
//! answer.
//!
//! One task runs at a time. Cancelling asks the agent to stop and commit what it has, rather than
//! discarding it, because a request to stop usually means priorities moved. Work is taken in the
//! order the queue hands it out, and nothing here decides what matters.

use std::time::{Duration, Instant};

use radicle::identity::RepoId;

use crate::{
    agent::{self, ActivityId, Coder},
    mcp::{ActivityContext, ActivityScope},
    member::Member,
    queue::{Task, TaskQueue},
    workspace::Workspaces,
};

// How long an interrupted task has to commit what it has before it is aborted. Unlike the task
// itself this is bounded, because committing takes seconds. Silence is bounded by [`agent::Stall`],
// on the same terms as every other agent the runtime runs.
const GRACE: Duration = Duration::from_secs(30);

/// Drives one member's building role through its queued work.
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
        queue: &TaskQueue,
        workspaces: &Workspaces,
        scope: &ActivityScope,
    ) -> Result<(), Error> {
        loop {
            let task = queue.next().await?;
            // Only a task which actually finished is forgotten. One which stalled, was aborted,
            // or failed stays marked taken, so the next start puts it back rather than losing it.
            self.carry(coder, queue, workspaces, scope, task).await?;
            queue.finish().await?;
        }
    }

    async fn carry(
        &self,
        coder: &(impl Coder + ?Sized),
        queue: &TaskQueue,
        workspaces: &Workspaces,
        scope: &ActivityScope,
        task: Task,
    ) -> Result<(), Error> {
        let activity = ActivityId::Task(task.id);
        let source = task.source.clone();
        let path = workspaces
            .prepare_task(&task)
            .await
            .map_err(|error| Error::Workspace(error.to_string()))?;
        scope
            .activate(ActivityContext {
                repository: self.repository,
                source: source.clone(),
                activity,
                workspace: path.clone(),
            })
            .await
            .map_err(Error::ActivityScope)?;
        tracing::info!(
            member = %self.member,
            repository = %self.repository,
            task = task.id,
            %source,
            workspace = %path.display(),
            "starting queued work"
        );
        let built = Self::build(coder, queue, task, path).await;
        scope.deactivate().await;
        let reconciled = scope.reconcile(activity).await;
        built?;
        reconciled.map_err(Error::PublicationRecovery)
    }

    // Carries out one task, stopping it when it is cancelled or when it stops making progress.
    async fn build(
        coder: &(impl Coder + ?Sized),
        queue: &TaskQueue,
        task: Task,
        workspace: std::path::PathBuf,
    ) -> Result<(), Error> {
        let identifier = task.id;
        let mut stall = agent::Stall::new(agent::Supervised::supervisor(coder));
        let mut stop: Option<Instant> = None;
        let building = coder.work(task, workspace);
        tokio::pin!(building);
        loop {
            tokio::select! {
                built = &mut building => {
                    return built.map_err(Error::from).inspect_err(|error| {
                        tracing::warn!(task = identifier, %error, "queued work failed");
                    });
                }
                () = queue.cancelled(identifier), if stop.is_none() => {
                    tracing::info!(task = identifier, "asking the running task to stop");
                    stop = Some(Instant::now() + GRACE);
                    coder.interrupt().await?;
                }
                silent = stall.silent() => {
                    // Once a stop was asked for the grace period decides, rather than whichever
                    // deadline happens to arrive first.
                    if let Some(deadline) = stop {
                        if Instant::now() >= deadline {
                            tracing::warn!(task = identifier, "task ignored the request to stop");
                            stall.abort().await?;

                            return Err(Error::Abandoned(identifier));
                        }
                    } else if silent {
                        stall.abort().await?;

                        return Err(Error::Stalled(identifier));
                    }
                }
            }
        }
    }
}

/// Error carrying out queued work.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading the task queue failed.
    #[error("task queue error: {0}")]
    Queue(#[from] crate::queue::Error),
    /// The building agent failed.
    #[error(transparent)]
    Coder(#[from] agent::Error),
    /// Preparing or cleaning the workspace failed.
    #[error("agent workspace failed: {0}")]
    Workspace(String),
    /// Activating the activity-scoped MCP capability failed.
    #[error("activity scope failed: {0}")]
    ActivityScope(String),
    /// Recovering durable MCP publications after a task failed.
    #[error("MCP publication recovery failed: {0}")]
    PublicationRecovery(String),
    /// The agent stopped making progress and was aborted.
    #[error("task {0} was stopped after making no progress")]
    Stalled(i64),
    /// The agent ignored the request to stop and was aborted.
    #[error("task {0} was stopped after ignoring the request to stop")]
    Abandoned(i64),
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr as _, sync::Arc};

    use radicle::{
        cob::ObjectId, crypto::PublicKey, git::UserInfo, identity::RepoId, node::Alias,
        storage::git::Storage,
    };

    use crate::{
        agent::{Coder, Error as AgentError, McpEndpoint},
        builder::{Builder, Error},
        manifest::QueuePosition,
        mcp::ActivityScope,
        member::{Member, Profile},
        queue::{Task, TaskQueue},
        watcher::Subject,
        workspace::Workspaces,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";
    const MEMBER: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    /// A builder which finishes only once it has been asked to stop.
    #[derive(Debug, Default)]
    struct InterruptibleCoder {
        stop: Arc<tokio::sync::Notify>,
        interruptions: Arc<tokio::sync::Mutex<usize>>,
    }

    impl crate::agent::Supervised for InterruptibleCoder {
        fn supervisor(&self) -> std::sync::Arc<crate::agent::Session> {
            std::sync::Arc::new(crate::agent::Session::new(crate::agent::Role::Coder))
        }
    }

    #[async_trait::async_trait]
    impl Coder for InterruptibleCoder {
        async fn start(
            &mut self,
            _profile: Profile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
            _layout: crate::agent::Layout,
        ) -> Result<(), AgentError> {
            Ok(())
        }

        async fn work(&self, _task: Task, _workspace: PathBuf) -> Result<(), AgentError> {
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
    async fn asks_the_running_task_to_stop_without_dropping_it() {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let source =
            Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse")).into();
        let _id = queue
            .push(crate::queue::TaskRequest::new(
                "write it".to_owned(),
                source,
                QueuePosition::End,
            ))
            .await
            .expect("task should be recorded")
            .task;
        let task = queue.next().await.expect("queue should be readable");
        let coder = InterruptibleCoder::default();

        let (built, cancelled) = tokio::join!(
            Builder::build(
                &coder,
                &queue,
                task.clone(),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-workspace"),
            ),
            queue.cancel(task.id),
        );

        built.expect("an interrupted task reports success once it has preserved its work");
        assert_eq!(cancelled.expect("cancellation should save").id, task.id);
        assert_eq!(
            *coder.interruptions.lock().await,
            1,
            "the role must be asked to stop rather than have its work dropped, so that it reaches \
             a point where nothing is lost"
        );
    }

    /// A task is forgotten only once it has actually been carried out. One which failed stays
    /// marked taken, so the next start puts it back rather than losing it without telling
    /// anybody.
    #[tokio::test]
    async fn keeps_a_failed_task_for_the_next_start() {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let source =
            Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse")).into();
        let _id = queue
            .push(crate::queue::TaskRequest::new(
                "write it".to_owned(),
                source,
                QueuePosition::End,
            ))
            .await
            .expect("task should be recorded")
            .task;

        let failed = Builder::new(Member::Nico, repository)
            .run(
                &InterruptibleCoder::default(),
                &queue,
                &workspaces(repository, queue.clone()),
                &ActivityScope::default(),
            )
            .await
            .expect_err("a task whose workspace cannot be prepared must fail");

        assert!(matches!(failed, Error::Workspace(_)), "{failed}");
        assert_eq!(
            queue.recover().await.expect("queue should be readable"),
            1,
            "a task which failed must still be there for the next start to pick up"
        );
    }

    /// Working copies over an empty storage, so preparing one fails the way a broken repository
    /// would.
    fn workspaces(repository: RepoId, queue: TaskQueue) -> Workspaces {
        let info = UserInfo {
            alias: Alias::new("builder-test"),
            key: PublicKey::from_str(MEMBER).expect("member should be valid"),
        };
        let storage = Storage::open(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/builder-failed-task/storage"),
            info.clone(),
        )
        .expect("empty storage should open");

        Workspaces::memory("builder-failed-task", storage, repository, info, queue)
            .expect("working copies should be created")
    }
}
