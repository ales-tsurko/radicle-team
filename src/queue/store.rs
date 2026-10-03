//! Task queue and handoff between one member's roles.
//!
//! A member answers messages while it builds, so the two cannot share one train of thought. The
//! talking role records work here instead of performing it, and the building role takes tasks from
//! here instead of acting on whatever the watcher noticed most recently. The queue survives
//! restarts because a task accepted in front of somebody is a promise, and losing it silently is
//! worse than refusing it.
//!
//! Each task carries a brief and the place the request came from. A brief is written by whoever
//! accepted the work, so the builder can start without reconstructing a discussion it never saw.
//!
//! Context and corrections survive interruptions. Completed requests retain only their outcome;
//! specifications and working notes are removed when a task is finished or cancelled.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use radicle::identity::RepoId;
use sqlite::State;

use crate::{
    agent::Source,
    database::Database,
    manifest::QueuePosition,
    member::Member,
    queue::{
        Artifact, ArtifactRole, ContextUpdate, Receipt, Task, TaskContext, TaskRequest, TaskState,
    },
    watcher::Subject,
};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS task_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repository TEXT NOT NULL,
        member TEXT NOT NULL,
        position INTEGER NOT NULL,
        brief TEXT NOT NULL,
        source TEXT NOT NULL,
        created INTEGER NOT NULL,
        taken INTEGER NOT NULL DEFAULT 0
    ) STRICT;

    CREATE INDEX IF NOT EXISTS task_queue_waiting
        ON task_queue (repository, member, position, id) WHERE taken = 0;

    CREATE TABLE IF NOT EXISTS task_contexts (
        task INTEGER PRIMARY KEY,
        data TEXT NOT NULL
    ) STRICT;

    CREATE TABLE IF NOT EXISTS task_requests (
        repository TEXT NOT NULL,
        member TEXT NOT NULL,
        request TEXT NOT NULL,
        task INTEGER NOT NULL,
        source TEXT NOT NULL,
        state TEXT NOT NULL,
        result TEXT,
        PRIMARY KEY (repository, member, request)
    ) STRICT;
";

/// Per-member task queue and the signals its roles exchange.
#[derive(Clone)]
pub struct TaskQueue {
    database: Database,
    repository: RepoId,
    member: Member,
    arrivals: Arc<tokio::sync::Notify>,
    cancellation: Arc<tokio::sync::watch::Sender<Option<i64>>>,
    running: Arc<tokio::sync::RwLock<Option<Task>>>,
    notes: Arc<tokio::sync::Mutex<Vec<(i64, String)>>>,
    // Publication discovery and creation must be atomic across this member's separate roles.
    pub(crate) creations: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for TaskQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskQueue")
            .field("repository", &self.repository)
            .field("member", &self.member)
            .finish_non_exhaustive()
    }
}

impl TaskQueue {
    /// Reads durable tasks in execution order without recovering or modifying the queue.
    pub(crate) fn inspect(
        connection: &sqlite::ConnectionThreadSafe,
        repository: RepoId,
        member: Member,
    ) -> sqlite::Result<Vec<(Task, bool)>> {
        if !Database::has_table(connection, "task_queue")? {
            return Ok(Vec::new());
        }
        let mut select = connection.prepare(
            "SELECT q.id, position, brief, source, taken, c.data AS context FROM task_queue q
             LEFT JOIN task_contexts c ON c.task = q.id
             WHERE repository = ?1 AND member = ?2 ORDER BY taken DESC, position, q.id",
        )?;
        select.bind((1, repository.to_string().as_str()))?;
        select.bind((2, member.id()))?;
        let mut tasks = Vec::new();
        while select.next()? == State::Row {
            tasks.push((Task::read(&select)?, select.read::<i64, _>("taken")? != 0));
        }
        Ok(tasks)
    }

    /// Opens the queue for one member and repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened or initialized.
    pub async fn open(path: &Path, repository: RepoId, member: Member) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let database = Database::open(path).await?;
        database.execute(SCHEMA).await?;
        Ok(Self::with_database(database, repository, member))
    }

    // Opens a queue that lives only as long as the process, for tests.
    #[cfg(test)]
    pub(crate) async fn memory(repository: RepoId, member: Member) -> Result<Self, Error> {
        let database = Database::memory().await?;
        database.execute(SCHEMA).await?;

        Ok(Self::with_database(database, repository, member))
    }

    /// Returns the storage behind the queue, so a test can take it away mid-run.
    ///
    /// Whether a write reached durable storage decides what a restart does, and a failure there has
    /// to be provable rather than reasoned about.
    #[cfg(test)]
    pub(crate) const fn database(&self) -> &Database {
        &self.database
    }

    pub(super) fn with_database(database: Database, repository: RepoId, member: Member) -> Self {
        Self {
            database,
            repository,
            member,
            arrivals: Arc::default(),
            cancellation: Arc::new(tokio::sync::watch::Sender::new(None)),
            running: Arc::default(),
            notes: Arc::default(),
            creations: Arc::default(),
        }
    }

    /// Records work once for a stable request, reusing related live work unless it is distinct.
    ///
    /// # Errors
    ///
    /// Returns an error when context validation or durable storage fails.
    pub async fn push(&self, mut request: TaskRequest) -> Result<Receipt, Error> {
        request.context.request.clone_from(&request.request);
        request.context.validate()?;
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        let receipt = self
            .database
            .call(move |connection| {
                Self::transaction(connection, |connection| {
                    Self::record_request(connection, &repository, &member, request)
                })
            })
            .await?;
        if receipt.state == TaskState::Waiting {
            self.arrivals.notify_one();
        }
        Ok(receipt)
    }

    fn record_request(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        mut request: TaskRequest,
    ) -> sqlite::Result<Receipt> {
        if let Some(key) = request.request.as_deref()
            && let Some(receipt) = Self::request_receipt(connection, repository, member, key)?
        {
            return Ok(receipt);
        }
        let source = request.source.to_string();
        let existing = if request.request.is_some() && !request.distinct {
            Self::related_task(connection, repository, member, &request.source)?
        } else {
            None
        };
        let receipt = match existing {
            Some((task, taken)) => Receipt {
                task: task.id,
                state: if taken {
                    TaskState::Running
                } else {
                    TaskState::Waiting
                },
                result: task.context.patch().map(|subject| subject.to_string()),
            },
            None => {
                if request.request.is_some()
                    && !request.distinct
                    && matches!(request.source, Source::Subject(_))
                    && let Some(result) =
                        Self::latest_result(connection, repository, member, &source)?
                {
                    request.context.relate(Artifact {
                        subject: result,
                        role: ArtifactRole::Produced,
                        purpose: "Existing deliverable for this task; revise it".to_owned(),
                    });
                }
                let mut insert = connection.prepare(
                    "INSERT INTO task_queue (repository, member, position, brief, source, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, unixepoch()) RETURNING id",
                )?;
                insert.bind((1, repository))?;
                insert.bind((2, member))?;
                insert.bind((3, Self::rank(request.position)))?;
                insert.bind((4, request.brief.as_str()))?;
                insert.bind((5, source.as_str()))?;
                insert.next()?;
                let task = insert.read::<i64, _>("id")?;
                drop(insert);
                Self::store_context(connection, task, &request.context)?;
                Receipt {
                    task,
                    state: TaskState::Waiting,
                    result: None,
                }
            }
        };
        if let Some(key) = request.request {
            Self::store_receipt(connection, repository, member, &key, &source, &receipt)?;
        }
        Ok(receipt)
    }

    fn store_receipt(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        request: &str,
        source: &str,
        receipt: &Receipt,
    ) -> sqlite::Result<()> {
        let mut insert = connection.prepare(
            "INSERT INTO task_requests (repository, member, request, task, source, state, result)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        insert.bind((1, repository))?;
        insert.bind((2, member))?;
        insert.bind((3, request))?;
        insert.bind((4, receipt.task))?;
        insert.bind((5, source))?;
        insert.bind((6, receipt.state.as_str()))?;
        insert.bind((7, receipt.result.as_deref()))?;
        insert.next()?;
        Ok(())
    }

    fn request_receipt(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        request: &str,
    ) -> sqlite::Result<Option<Receipt>> {
        let mut select = connection.prepare(
            "SELECT task, state, result FROM task_requests
             WHERE repository = ?1 AND member = ?2 AND request = ?3",
        )?;
        select.bind((1, repository))?;
        select.bind((2, member))?;
        select.bind((3, request))?;
        if select.next()? != State::Row {
            return Ok(None);
        }
        Ok(Some(Receipt {
            task: select.read(0)?,
            state: TaskState::read(&select.read::<String, _>(1)?),
            result: select.read(2)?,
        }))
    }

    fn latest_result(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        source: &str,
    ) -> sqlite::Result<Option<String>> {
        let mut select = connection.prepare(
            "SELECT result FROM task_requests WHERE repository = ?1 AND member = ?2
             AND source = ?3 AND state = 'completed' AND result IS NOT NULL
             ORDER BY task DESC LIMIT 1",
        )?;
        select.bind((1, repository))?;
        select.bind((2, member))?;
        select.bind((3, source))?;
        if select.next()? == State::Row {
            select.read(0)
        } else {
            Ok(None)
        }
    }

    fn related_task(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        source: &Source,
    ) -> sqlite::Result<Option<(Task, bool)>> {
        // A channel is a place to talk, not the identity of a task. Its messages can request
        // unrelated work; only replaying the same message is an implicit duplicate.
        if matches!(source, Source::Chat(_)) {
            return Ok(None);
        }
        let mut select = connection.prepare(
            "SELECT q.id, position, brief, source, taken, c.data AS context FROM task_queue q
             LEFT JOIN task_contexts c ON c.task = q.id
             WHERE repository = ?1 AND member = ?2 ORDER BY q.id",
        )?;
        select.bind((1, repository))?;
        select.bind((2, member))?;
        while select.next()? == State::Row {
            let task = Task::read(&select)?;
            if task.source == *source || task.target() == *source {
                return Ok(Some((task, select.read::<i64, _>("taken")? != 0)));
            }
        }
        Ok(None)
    }

    fn transaction<T>(
        connection: &sqlite::ConnectionThreadSafe,
        operation: impl FnOnce(&sqlite::ConnectionThreadSafe) -> sqlite::Result<T>,
    ) -> sqlite::Result<T> {
        connection.execute("BEGIN IMMEDIATE")?;
        match operation(connection) {
            Ok(value) => {
                connection.execute("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _rollback = connection.execute("ROLLBACK");
                Err(error)
            }
        }
    }

    const fn rank(position: QueuePosition) -> i64 {
        match position {
            QueuePosition::Front => 0,
            QueuePosition::Next => 1,
            QueuePosition::End => 2,
        }
    }

    fn store_context(
        connection: &sqlite::ConnectionThreadSafe,
        task: i64,
        context: &TaskContext,
    ) -> sqlite::Result<()> {
        context.validate().map_err(Task::stored_error)?;
        let data = serde_json::to_string(context).map_err(Task::stored_error)?;
        let mut insert = connection.prepare(
            "INSERT INTO task_contexts (task, data) VALUES (?1, ?2)
             ON CONFLICT(task) DO UPDATE SET data = excluded.data",
        )?;
        insert.bind((1, task))?;
        insert.bind((2, data.as_str()))?;
        insert.next()?;
        Ok(())
    }

    /// Waits for the task that should run next and records it as running.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read or updated.
    pub async fn next(&self) -> Result<Task, Error> {
        loop {
            let arrival = self.arrivals.notified();
            tokio::pin!(arrival);
            arrival.as_mut().enable();
            if let Some(task) = self.take().await? {
                let mut running = self.running.write().await;
                let task = self.get(task.id).await?;
                *running = Some(task.clone());
                return Ok(task);
            }
            arrival.await;
        }
    }

    /// Records that a task finished, removing it and discarding anything said to it.
    ///
    /// # Errors
    ///
    /// Returns an error when the finished task cannot be removed, which would otherwise leave it
    /// looking like work an earlier run abandoned.
    pub async fn finish(&self) -> Result<(), Error> {
        let mut running = self.running.write().await;
        if let Some(task) = running.as_ref() {
            let id = task.id;
            self.database
                .call(move |connection| {
                    Self::transaction(connection, |connection| Self::retire(connection, id, None))
                })
                .await?;
        }
        *running = None;
        self.notes.lock().await.clear();
        Ok(())
    }

    fn retire(
        connection: &sqlite::ConnectionThreadSafe,
        id: i64,
        state: Option<&str>,
    ) -> sqlite::Result<()> {
        let task =
            Self::find_task(connection, id)?.ok_or_else(|| Task::stored_error("task not found"))?;
        let state = state.unwrap_or(if task.context.cancelled {
            "cancelled"
        } else {
            "completed"
        });
        let result = match task.target() {
            Source::Subject(Subject::Patch(patch)) => Some(Subject::Patch(patch).to_string()),
            _ => None,
        };
        let mut update = connection
            .prepare("UPDATE task_requests SET state = ?2, result = ?3 WHERE task = ?1")?;
        update.bind((1, id))?;
        update.bind((2, state))?;
        update.bind((3, result.as_deref()))?;
        update.next()?;
        for table in ["task_contexts", "task_queue"] {
            let mut delete = connection.prepare(format!(
                "DELETE FROM {table} WHERE {} = ?1",
                if table == "task_contexts" {
                    "task"
                } else {
                    "id"
                }
            ))?;
            delete.bind((1, id))?;
            delete.next()?;
        }
        Ok(())
    }

    /// Puts tasks which were running when the process stopped back at the front of the queue.
    ///
    /// A task is marked taken while it runs and removed when it finishes, so anything still marked
    /// taken at startup belonged to a run which died. Left alone it would sit there forever and
    /// nobody would be told, which turns one failure into a silent second one.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read or updated.
    pub async fn recover(&self) -> Result<usize, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        let recovered = self
            .database
            .call(move |connection| {
                Self::transaction(connection, |connection| {
                    Self::recover_taken(connection, &repository, &member)
                })
            })
            .await?;
        if recovered > 0 {
            tracing::warn!(
                tasks = recovered,
                "requeued work which was interrupted by an earlier run"
            );
            self.arrivals.notify_waiters();
        }

        Ok(recovered)
    }

    fn recover_taken(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
    ) -> sqlite::Result<usize> {
        let mut requests = connection.prepare(
            "UPDATE task_requests SET state = 'waiting'
             WHERE repository = ?1 AND member = ?2 AND state = 'running'
             AND task IN (SELECT id FROM task_queue WHERE taken = 1)",
        )?;
        requests.bind((1, repository))?;
        requests.bind((2, member))?;
        requests.next()?;
        let mut update = connection.prepare(
            "UPDATE task_queue SET taken = 0, position = ?3
             WHERE repository = ?1 AND member = ?2 AND taken = 1",
        )?;
        update.bind((1, repository))?;
        update.bind((2, member))?;
        update.bind((3, Self::rank(QueuePosition::Front)))?;
        update.next()?;
        Ok(connection.change_count())
    }

    /// Records an artifact created by the runtime while preserving the task's original source.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is absent or already has a different produced patch.
    pub async fn produced(&self, task: i64, subject: Subject) -> Result<Task, Error> {
        self.change_context(task, move |context| {
            if matches!(subject, Subject::Patch(_))
                && let Some(patch) = context.patch()
                && patch != subject
            {
                return Err(Error::ExistingPatch(patch.to_string()));
            }
            context.relate(Artifact {
                subject: subject.to_string(),
                role: ArtifactRole::Produced,
                purpose: "Published by this task".to_owned(),
            });
            Ok(())
        })
        .await
    }

    /// Reads a live task and its durable context, whether it is running or waiting.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails or the task no longer exists.
    pub async fn get(&self, task: i64) -> Result<Task, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                let mut owned = connection.prepare(
                    "SELECT id FROM task_queue WHERE id = ?1 AND repository = ?2 AND member = ?3",
                )?;
                owned.bind((1, task))?;
                owned.bind((2, repository.as_str()))?;
                owned.bind((3, member.as_str()))?;
                if owned.next()? != State::Row {
                    return Ok(None);
                }
                Self::find_task(connection, task)
            })
            .await?
            .ok_or(Error::MissingTask(task))
    }

    fn find_task(
        connection: &sqlite::ConnectionThreadSafe,
        task: i64,
    ) -> sqlite::Result<Option<Task>> {
        let mut select = connection.prepare(
            "SELECT q.id, position, brief, source, c.data AS context FROM task_queue q
             LEFT JOIN task_contexts c ON c.task = q.id WHERE q.id = ?1",
        )?;
        select.bind((1, task))?;
        if select.next()? == State::Row {
            Task::read(&select).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Appends member-selected references and notes without changing runtime provenance.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid references, runtime-owned relationships, or absent work.
    pub async fn update_context(&self, task: i64, update: ContextUpdate) -> Result<Task, Error> {
        let changed = self
            .change_context(task, move |context| context.update(update))
            .await?;
        if self
            .running()
            .await
            .is_some_and(|running| running.id == task)
        {
            self.notes.lock().await.push((
                task,
                "Task context changed; read get_task_context before your next step".to_owned(),
            ));
        }
        Ok(changed)
    }

    async fn change_context(
        &self,
        task: i64,
        change: impl FnOnce(&mut TaskContext) -> Result<(), Error> + Send + 'static,
    ) -> Result<Task, Error> {
        self.get(task).await?;
        let changed = self
            .database
            .call(move |connection| {
                Self::transaction(connection, |connection| {
                    let mut row = Self::find_task(connection, task)?
                        .ok_or_else(|| Task::stored_error("task no longer exists"))?;
                    if let Err(error) = change(&mut row.context) {
                        return Ok(Err(error));
                    }
                    Self::store_context(connection, task, &row.context)?;
                    Ok(Ok(row))
                })
            })
            .await??;
        let mut running = self.running.write().await;
        if running.as_ref().is_some_and(|running| running.id == task) {
            *running = Some(changed.clone());
        }
        Ok(changed)
    }

    /// Returns the task currently being carried out.
    pub async fn running(&self) -> Option<Task> {
        self.running.read().await.clone()
    }

    /// Saves a correction for one running or waiting task.
    ///
    /// # Errors
    ///
    /// Returns an error when the named task is absent or durable storage fails.
    pub async fn note(&self, task: i64, note: String) -> Result<Task, Error> {
        let retained = note.clone();
        let changed = self
            .change_context(task, move |context| {
                context.update(ContextUpdate {
                    notes: vec![retained],
                    ..ContextUpdate::default()
                })
            })
            .await?;
        if self
            .running()
            .await
            .is_some_and(|running| running.id == task)
        {
            self.notes.lock().await.push((task, note));
        }
        Ok(changed)
    }

    /// Takes every note recorded since the last time they were read.
    pub async fn notes(&self, task: i64) -> Vec<String> {
        let mut notes = self.notes.lock().await;
        let mut delivered = Vec::new();
        notes.retain(|(target, note)| {
            if *target == task {
                delivered.push(note.clone());
                false
            } else {
                true
            }
        });
        delivered
    }

    /// Cancels one waiting task or asks the named running task to preserve its work and stop.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is absent or its cancellation cannot be saved.
    pub async fn cancel(&self, task: i64) -> Result<Task, Error> {
        self.get(task).await?;
        let changed = self
            .database
            .call(move |connection| {
                Self::transaction(connection, |connection| Self::cancel_task(connection, task))
            })
            .await?;
        if changed.1 {
            let mut running = self.running.write().await;
            if running.as_ref().is_some_and(|running| running.id == task) {
                *running = Some(changed.0.clone());
            }
            let _previous = self.cancellation.send_replace(Some(task));
        }
        Ok(changed.0)
    }

    fn cancel_task(
        connection: &sqlite::ConnectionThreadSafe,
        task: i64,
    ) -> sqlite::Result<(Task, bool)> {
        let mut row = Self::find_task(connection, task)?
            .ok_or_else(|| Task::stored_error("task no longer exists"))?;
        row.context.cancelled = true;
        Self::store_context(connection, task, &row.context)?;
        let mut select = connection.prepare("SELECT taken FROM task_queue WHERE id = ?1")?;
        select.bind((1, task))?;
        select.next()?;
        let taken = select.read::<i64, _>(0)? != 0;
        drop(select);
        if !taken {
            Self::retire(connection, task, Some("cancelled"))?;
        }
        Ok((row, taken))
    }

    /// Waits until somebody asks `task` in particular to stop.
    pub async fn cancelled(&self, task: i64) {
        let mut cancellations = self.cancellation.subscribe();
        while *cancellations.borrow_and_update() != Some(task) {
            cancellations
                .changed()
                .await
                .expect("the queue holds the sender, so it outlives every subscriber");
        }
    }

    // Takes the task that should run next, marking it as taken.
    pub(super) async fn take(&self) -> Result<Option<Task>, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                connection.execute("BEGIN IMMEDIATE")?;
                let taken = Self::take_transaction(connection, &repository, &member);
                taken
                    .and_then(|taken| {
                        connection.execute("COMMIT")?;
                        Ok(taken)
                    })
                    .inspect_err(|_| {
                        let _rollback = connection.execute("ROLLBACK");
                    })
            })
            .await
            .map_err(Error::from)
    }

    /// Returns how many tasks are still waiting.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read.
    pub async fn waiting(&self) -> Result<usize, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT COUNT(*) FROM task_queue
                     WHERE repository = ?1 AND member = ?2 AND taken = 0",
                )?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                statement.next()?;
                statement.read::<i64, _>(0)
            })
            .await
            .map(|waiting| waiting.unsigned_abs() as usize)
            .map_err(Error::from)
    }

    /// Returns the identifier of every task this member still has, running or waiting.
    ///
    /// A row is removed only once its task was carried out, so this is also which branches matter.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read.
    pub async fn live(&self) -> Result<BTreeSet<i64>, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection
                    .prepare("SELECT id FROM task_queue WHERE repository = ?1 AND member = ?2")?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                let mut live = BTreeSet::new();
                while statement.next()? == State::Row {
                    let _added = live.insert(statement.read::<i64, _>("id")?);
                }
                Ok(live)
            })
            .await
            .map_err(Error::from)
    }

    /// Returns every task still waiting, in the order they will run.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read.
    pub async fn pending(&self) -> Result<Vec<Task>, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT q.id, position, brief, source, c.data AS context FROM task_queue q
                     LEFT JOIN task_contexts c ON c.task = q.id
                     WHERE repository = ?1 AND member = ?2 AND taken = 0
                     ORDER BY position ASC, q.id ASC",
                )?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                let mut tasks = Vec::new();
                while statement.next()? == State::Row {
                    tasks.push(Task::read(&statement)?);
                }
                Ok(tasks)
            })
            .await
            .map_err(Error::from)
    }

    fn take_transaction(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
    ) -> sqlite::Result<Option<Task>> {
        let mut statement = connection.prepare(
            "SELECT q.id, position, brief, source, c.data AS context FROM task_queue q
             LEFT JOIN task_contexts c ON c.task = q.id
             WHERE repository = ?1 AND member = ?2 AND taken = 0
             ORDER BY position ASC, q.id ASC
             LIMIT 1",
        )?;
        statement.bind((1, repository))?;
        statement.bind((2, member))?;
        if statement.next()? != State::Row {
            return Ok(None);
        }
        let task = Task::read(&statement)?;
        drop(statement);

        let mut update = connection.prepare("UPDATE task_queue SET taken = 1 WHERE id = ?1")?;
        update.bind((1, task.id))?;
        update.next()?;
        let mut requests =
            connection.prepare("UPDATE task_requests SET state = 'running' WHERE task = ?1")?;
        requests.bind((1, task.id))?;
        requests.next()?;
        Ok(Some(task))
    }
}
/// Error returned by the task queue.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The queue database directory could not be prepared.
    #[error("task queue I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding durable task context failed.
    #[error("task context encoding failed: {0}")]
    Context(#[from] serde_json::Error),
    /// A context reference or provenance update is invalid.
    #[error("invalid task context: {0}")]
    InvalidContext(String),
    /// The requested task is no longer active.
    #[error("task {0} is not running or waiting; check task_status before correcting it")]
    MissingTask(i64),
    /// A task already produced a patch.
    #[error("this task already produced {0}; use update_patch to publish a revision")]
    ExistingPatch(String),
    /// The queue database could not be accessed.
    #[error("task queue database error: {0}")]
    Database(String),
}

impl From<crate::database::Error> for Error {
    fn from(error: crate::database::Error) -> Self {
        Self::Database(error.to_string())
    }
}
