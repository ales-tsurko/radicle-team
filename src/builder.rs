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
// A broken provider stream must not spin a fresh process or stop the member's other roles.
const RETRY_DELAY: Duration = Duration::from_secs(10);

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
        inbox: &crate::delivery::Inbox,
    ) -> Result<(), Error> {
        let watch = agent::Supervised::supervisor(coder);
        loop {
            let mut task = queue.next().await?;
            watch.handling(&task.source, Some(task.id)).await;
            let identifier = task.id;
            inbox
                .task(identifier, crate::delivery::State::Processing, None)
                .await?;
            loop {
                match self.carry(coder, queue, workspaces, scope, task).await {
                    Ok(()) => break,
                    Err(
                        error @ (Error::Coder(_)
                        | Error::Unpublished(_)
                        | Error::Stalled(_)
                        | Error::Abandoned(_)),
                    ) => {
                        watch.retrying(error.to_string()).await;
                        inbox
                            .task(
                                identifier,
                                crate::delivery::State::Failed,
                                Some(error.to_string()),
                            )
                            .await?;
                        tracing::warn!(
                            task = identifier,
                            %error,
                            retry_in_seconds = RETRY_DELAY.as_secs(),
                            "retrying queued work; the member's other roles remain active"
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                        // Publications and notes from the failed attempt belong to this same task.
                        task = queue.get(identifier).await?;
                        inbox
                            .task(identifier, crate::delivery::State::Processing, None)
                            .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            // Only finished work is forgotten. Stalled or aborted work stays marked taken for
            // the next start to recover.
            queue.finish().await?;
            inbox.wake();
            watch.handled().await;
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
                delivery: task.context.activity.clone(),
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
        let identifier = task.id;
        let built = Self::build(coder, queue, task, path).await;
        scope.deactivate().await;
        scope
            .reconcile(activity)
            .await
            .map_err(Error::PublicationRecovery)?;
        built?;
        let task = queue.get(identifier).await?;
        if let Err(error) = workspaces.verify_completion(&task).await {
            queue.note(identifier, error.to_string()).await?;
            return Err(Error::Unpublished(error));
        }
        Ok(())
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
    /// The agent ended before its task's result was published.
    #[error(transparent)]
    Unpublished(crate::workspace::Error),
    /// Recording delivery state failed.
    #[error("delivery state failed: {0}")]
    Delivery(#[from] crate::delivery::Error),
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
        home::MemberHome,
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

        let inbox = queue.inbox().await.expect("inbox should open");
        let failed = Builder::new(Member::Nico, repository)
            .run(
                &InterruptibleCoder::default(),
                &queue,
                &workspaces(repository, queue.clone()),
                &ActivityScope::default(),
                &inbox,
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

    #[derive(Debug)]
    struct FailingOnceCoder {
        queue: TaskQueue,
        attempts: tokio::sync::Mutex<usize>,
        started: tokio::sync::mpsc::UnboundedSender<Task>,
    }

    impl crate::agent::Supervised for FailingOnceCoder {
        fn supervisor(&self) -> Arc<crate::agent::Session> {
            Arc::new(crate::agent::Session::new(crate::agent::Role::Coder))
        }
    }

    #[async_trait::async_trait]
    impl Coder for FailingOnceCoder {
        async fn start(
            &mut self,
            _profile: Profile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
            _layout: crate::agent::Layout,
        ) -> Result<(), AgentError> {
            Ok(())
        }

        async fn work(&self, task: Task, _workspace: PathBuf) -> Result<(), AgentError> {
            let attempt = {
                let mut attempts = self.attempts.lock().await;
                *attempts += 1;
                *attempts
            };
            self.started
                .send(task.clone())
                .expect("observer should remain open");
            match attempt {
                1 => {
                    self.queue
                        .note(task.id, "Retained before the stream failed".to_owned())
                        .await
                        .map_err(AgentError::new)?;
                    Err(AgentError::new(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "provider stream ended prematurely",
                    )))
                }
                2 => Ok(()),
                _ => std::future::pending().await,
            }
        }

        async fn interrupt(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn retries_adapter_failure_with_retained_context_before_taking_the_next_task() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let (repository, queue, copies) = retry_workspaces(root.path()).await;
        let source = Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse"));
        let mut tasks = Vec::new();
        for brief in ["first", "second"] {
            tasks.push(
                queue
                    .push(crate::queue::TaskRequest::new(
                        brief.to_owned(),
                        source.into(),
                        QueuePosition::End,
                    ))
                    .await
                    .expect("task should be recorded")
                    .task,
            );
        }
        let [first, second]: [i64; 2] = tasks.try_into().expect("two tasks should be queued");
        let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let coder = FailingOnceCoder {
            queue: queue.clone(),
            attempts: tokio::sync::Mutex::new(0),
            started,
        };
        let builder = Builder::new(Member::Nico, repository);
        let scope = ActivityScope::default();
        let inbox = crate::delivery::Inbox::memory()
            .await
            .expect("inbox should open");
        let running = builder.run(&coder, &queue, &copies, &scope, &inbox);
        tokio::pin!(running);
        let attempts = tokio::select! {
            result = &mut running => {
                result.expect("an adapter failure must not end the builder");
                Vec::new()
            }
            attempts = async {
                let initial = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    observed.recv(),
                )
                .await
                .expect("the first task should start")
                .expect("builder should stay active");

                // Freeze only the backoff. Git subprocesses run with the clock resumed.
                tokio::time::pause();
                let tick = std::time::Duration::from_millis(1);
                let before_retry = super::RETRY_DELAY.checked_sub(tick).expect("nonzero backoff");
                tokio::time::sleep(before_retry).await;
                assert!(
                    matches!(observed.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
                    "the failed task must not retry before the backoff expires"
                );
                tokio::time::advance(tick).await;
                tokio::time::resume();

                let mut attempts = vec![initial];
                for _ in 0..2 {
                    let attempt = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        observed.recv(),
                    )
                    .await
                    .expect("each attempt should make progress after the virtual backoff")
                    .expect("builder should stay active");
                    attempts.push(attempt);
                }
                attempts
            } => attempts,
        };
        let [initial, retried, following]: [Task; 3] = attempts
            .try_into()
            .expect("three attempts should be observed");
        assert_eq!(
            [initial.id, retried.id, following.id],
            [first, first, second]
        );
        assert_eq!(retried.context.notes, ["Retained before the stream failed"]);
        assert!(matches!(
            queue.get(first).await,
            Err(crate::queue::Error::MissingTask(_))
        ));
    }

    async fn retry_workspaces(root: &std::path::Path) -> (RepoId, TaskQueue, Workspaces) {
        use radicle::{crypto::Seed, identity::doc::Visibility, profile::Home};

        let profile = radicle::profile::Profile::init(
            Home::new(root.join("identity")).expect("profile home should be created"),
            Alias::new("builder-test"),
            None,
            Seed::new(rand::random()),
        )
        .expect("profile should initialize");
        let repository = {
            let checkout =
                git2::Repository::init(root.join("repo")).expect("repository should initialize");
            let signature = git2::Signature::now("Builder Test", "builder@example.com")
                .expect("signature should be valid");
            let tree = checkout
                .index()
                .expect("index should open")
                .write_tree()
                .expect("tree should be written");
            let tree = checkout.find_tree(tree).expect("tree should exist");
            checkout
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "Initial",
                    &tree,
                    &[],
                )
                .expect("initial commit should be written");
            checkout
                .set_head("refs/heads/main")
                .expect("main should be selected");
            let signer = profile.signer().expect("signer should load");
            radicle::rad::init(
                &checkout,
                "builder-test".try_into().expect("name should be valid"),
                "Builder test",
                "main".try_into().expect("branch should be valid"),
                Visibility::default(),
                &signer,
                &profile.storage,
            )
            .expect("Radicle repository should initialize")
            .0
        };
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let home = MemberHome::at(root.join("member"));
        home.create().expect("member home should be created");
        let info = UserInfo {
            alias: Alias::new("builder-test"),
            key: *profile.id(),
        };
        let copies = Workspaces::new(
            Member::Nico,
            home,
            profile.storage,
            repository,
            info,
            queue.clone(),
        );
        copies
            .create()
            .await
            .expect("working copies should be created");
        (repository, queue, copies)
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
