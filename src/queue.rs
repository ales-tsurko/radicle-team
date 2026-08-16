//! Work queue and handoff between one member's roles.
//!
//! A member answers messages while it builds, so the two cannot share one train of thought. The
//! talking role records work here instead of performing it, and the building role takes jobs from
//! here instead of acting on whatever the watcher noticed most recently. The queue survives
//! restarts because a job accepted in front of somebody is a promise, and losing it silently is
//! worse than refusing it.
//!
//! Each job carries a brief and the place the request came from. A brief is written by whoever
//! accepted the work, so the builder can start without reconstructing a discussion it never saw.
//!
//! Waiting work is durable, but everything the roles say to each other while a job runs is not: a
//! note, a request to stop, and which job is running only mean anything to the process holding
//! them. They live here anyway, because they are the same conversation as the queue and splitting
//! them would mean handing each role two objects instead of one.

use std::{collections::BTreeSet, path::Path, str::FromStr as _, sync::Arc};

use radicle::identity::RepoId;
use sqlite::State;

use crate::{agent::Source, database::Database, manifest::QueuePosition, member::Member};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS work_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repository TEXT NOT NULL,
        member TEXT NOT NULL,
        position INTEGER NOT NULL,
        brief TEXT NOT NULL,
        source TEXT NOT NULL,
        created INTEGER NOT NULL,
        taken INTEGER NOT NULL DEFAULT 0
    ) STRICT;

    CREATE INDEX IF NOT EXISTS work_queue_waiting
        ON work_queue (repository, member, position, id) WHERE taken = 0;
";

/// One unit of work waiting for a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Database identifier, assigned when the job is recorded.
    pub id: i64,
    /// What to do, in enough detail to begin without rereading the discussion.
    pub brief: String,
    /// Place the request came from.
    pub source: Source,
    /// Where the job joined the queue.
    pub position: QueuePosition,
}

/// Per-member work queue and the signals its roles exchange.
#[derive(Clone)]
pub struct WorkQueue {
    database: Database,
    repository: RepoId,
    member: Member,
    arrivals: Arc<tokio::sync::Notify>,
    cancellation: Arc<tokio::sync::watch::Sender<Option<i64>>>,
    running: Arc<tokio::sync::RwLock<Option<Job>>>,
    notes: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl std::fmt::Debug for WorkQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkQueue")
            .field("repository", &self.repository)
            .field("member", &self.member)
            .finish_non_exhaustive()
    }
}

impl WorkQueue {
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
    #[cfg(all(test, not(miri)))]
    pub(crate) async fn memory(repository: RepoId, member: Member) -> Result<Self, Error> {
        let database = Database::memory().await?;
        database.execute(SCHEMA).await?;

        Ok(Self::with_database(database, repository, member))
    }

    /// Returns the storage behind the queue, so a test can take it away mid-run.
    ///
    /// Whether a write reached durable storage decides what a restart does, and a failure there
    /// has to be provable rather than reasoned about.
    #[cfg(all(test, not(miri)))]
    pub(crate) const fn database(&self) -> &Database {
        &self.database
    }

    fn with_database(database: Database, repository: RepoId, member: Member) -> Self {
        Self {
            database,
            repository,
            member,
            arrivals: Arc::default(),
            cancellation: Arc::new(tokio::sync::watch::Sender::new(None)),
            running: Arc::default(),
            notes: Arc::default(),
        }
    }

    /// Records `brief` as work waiting at `position` and returns its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the job cannot be written.
    pub async fn push(
        &self,
        brief: String,
        source: Source,
        position: QueuePosition,
    ) -> Result<i64, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        let source = source.to_string();
        let rank = rank(position);
        let id = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "INSERT INTO work_queue (repository, member, position, brief, source, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
                     RETURNING id",
                )?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                statement.bind((3, rank))?;
                statement.bind((4, brief.as_str()))?;
                statement.bind((5, source.as_str()))?;
                statement.next()?;
                statement.read::<i64, _>("id")
            })
            .await?;
        self.arrivals.notify_one();

        Ok(id)
    }

    /// Waits for the job that should run next and records it as running.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read or updated.
    pub async fn next(&self) -> Result<Job, Error> {
        loop {
            let arrival = self.arrivals.notified();
            tokio::pin!(arrival);
            arrival.as_mut().enable();
            if let Some(job) = self.take().await? {
                *self.running.write().await = Some(job.clone());
                return Ok(job);
            }
            arrival.await;
        }
    }

    /// Records that a job finished, removing it and discarding anything said to it.
    ///
    /// # Errors
    ///
    /// Returns an error when the finished job cannot be removed, which would otherwise leave it
    /// looking like work an earlier run abandoned.
    pub async fn finish(&self) -> Result<(), Error> {
        let done = self.running.write().await.take();
        self.notes.lock().await.clear();
        match done {
            Some(job) => self.remove(job.id).await,
            None => Ok(()),
        }
    }

    /// Puts jobs which were running when the process stopped back at the front of the queue.
    ///
    /// A job is marked taken while it runs and removed when it finishes, so anything still marked
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
                let mut update = connection.prepare(
                    "UPDATE work_queue SET taken = 0, position = ?3
                     WHERE repository = ?1 AND member = ?2 AND taken = 1",
                )?;
                update.bind((1, repository.as_str()))?;
                update.bind((2, member.as_str()))?;
                update.bind((3, rank(QueuePosition::Front)))?;
                while update.next()? != State::Done {}

                Ok(connection.change_count())
            })
            .await?;
        if recovered > 0 {
            tracing::warn!(
                jobs = recovered,
                "requeued work which was interrupted by an earlier run"
            );
            self.arrivals.notify_waiters();
        }

        Ok(recovered)
    }

    /// Records that a job is now about `source` rather than whatever it started on.
    ///
    /// A job taken from an issue becomes a job about the patch it produced. Written down because
    /// the job may still fail after that, and the run which picks it up again has no other way to
    /// know: it would otherwise start over on the issue and leave the patch behind.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be updated.
    pub async fn retarget(&self, job: i64, source: Source) -> Result<(), Error> {
        let target = source.to_string();
        self.database
            .call(move |connection| {
                let mut update =
                    connection.prepare("UPDATE work_queue SET source = ?2 WHERE id = ?1")?;
                update.bind((1, job))?;
                update.bind((2, target.as_str()))?;
                while update.next()? != State::Done {}

                Ok(())
            })
            .await?;
        if let Some(running) = self.running.write().await.as_mut() {
            running.source = source;
        }

        Ok(())
    }

    async fn remove(&self, job: i64) -> Result<(), Error> {
        self.database
            .call(move |connection| {
                let mut delete = connection.prepare("DELETE FROM work_queue WHERE id = ?1")?;
                delete.bind((1, job))?;
                while delete.next()? != State::Done {}

                Ok(())
            })
            .await
            .map_err(Error::from)
    }

    /// Returns the job currently being carried out.
    pub async fn running(&self) -> Option<Job> {
        self.running.read().await.clone()
    }

    /// Records a correction for the running job, returning the job it will reach.
    pub async fn note(&self, note: String) -> Option<Job> {
        let running = self.running().await;
        if running.is_some() {
            self.notes.lock().await.push(note);
        }

        running
    }

    /// Takes every note recorded since the last time they were read.
    pub async fn notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().await)
    }

    /// Asks the running job to stop and preserve what it has, returning the job reached.
    pub async fn cancel(&self) -> Option<Job> {
        let running = self.running().await;
        if let Some(job) = &running {
            let _previous = self.cancellation.send_replace(Some(job.id));
        }

        running
    }

    /// Waits until somebody asks `job` in particular to stop.
    pub async fn cancelled(&self, job: i64) {
        let mut cancellations = self.cancellation.subscribe();
        while *cancellations.borrow_and_update() != Some(job) {
            cancellations
                .changed()
                .await
                .expect("the queue holds the sender, so it outlives every subscriber");
        }
    }

    // Takes the job that should run next, marking it as taken.
    async fn take(&self) -> Result<Option<Job>, Error> {
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

    /// Returns how many jobs are still waiting.
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
                    "SELECT COUNT(*) FROM work_queue
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

    /// Returns the identifier of every job this member still has, running or waiting.
    ///
    /// A row is removed only once its job was carried out, so this is also which branches matter.
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
                    .prepare("SELECT id FROM work_queue WHERE repository = ?1 AND member = ?2")?;
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

    /// Returns every job still waiting, in the order they will run.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read.
    pub async fn pending(&self) -> Result<Vec<Job>, Error> {
        let repository = self.repository.to_string();
        let member = self.member.id().to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT id, position, brief, source FROM work_queue
                     WHERE repository = ?1 AND member = ?2 AND taken = 0
                     ORDER BY position ASC, id ASC",
                )?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                let mut jobs = Vec::new();
                while statement.next()? == State::Row {
                    jobs.push(read_job(&statement)?);
                }
                Ok(jobs)
            })
            .await
            .map_err(Error::from)
    }

    fn take_transaction(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
    ) -> sqlite::Result<Option<Job>> {
        let mut statement = connection.prepare(
            "SELECT id, position, brief, source FROM work_queue
             WHERE repository = ?1 AND member = ?2 AND taken = 0
             ORDER BY position ASC, id ASC
             LIMIT 1",
        )?;
        statement.bind((1, repository))?;
        statement.bind((2, member))?;
        if statement.next()? != State::Row {
            return Ok(None);
        }
        let job = read_job(&statement)?;
        drop(statement);

        let mut update = connection.prepare("UPDATE work_queue SET taken = 1 WHERE id = ?1")?;
        update.bind((1, job.id))?;
        update.next()?;
        Ok(Some(job))
    }
}

fn read_job(statement: &sqlite::Statement<'_>) -> sqlite::Result<Job> {
    let source = statement.read::<String, _>("source")?;
    Ok(Job {
        id: statement.read::<i64, _>("id")?,
        brief: statement.read::<String, _>("brief")?,
        source: Source::from_str(&source).map_err(|error| sqlite::Error {
            code: None,
            message: Some(error.to_string()),
        })?,
        position: position(statement.read::<i64, _>("position")?),
    })
}

const fn rank(position: QueuePosition) -> i64 {
    match position {
        QueuePosition::Front => 0,
        QueuePosition::Next => 1,
        QueuePosition::End => 2,
    }
}

const fn position(rank: i64) -> QueuePosition {
    match rank {
        0 => QueuePosition::Front,
        1 => QueuePosition::Next,
        _ => QueuePosition::End,
    }
}

/// Error returned by the work queue.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The queue database directory could not be prepared.
    #[error("work queue I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The queue database could not be accessed.
    #[error("work queue database error: {0}")]
    Database(String),
}

impl From<crate::database::Error> for Error {
    fn from(error: crate::database::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{str::FromStr as _, time::Duration};

    use radicle::{cob::ObjectId, identity::RepoId};

    use crate::{
        agent::Source, database::Database, manifest::QueuePosition, member::Member,
        queue::WorkQueue, watcher::Subject,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";
    const PRODUCED: &str = "2222222222222222222222222222222222222222";

    async fn queue() -> WorkQueue {
        let database = Database::memory().await.expect("database should open");
        database
            .execute(super::SCHEMA)
            .await
            .expect("schema should apply");
        WorkQueue::with_database(
            database,
            RepoId::from_str(REPOSITORY).expect("repository should parse"),
            Member::Nico,
        )
    }

    fn subject() -> Source {
        Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse")).into()
    }

    /// A job which produced a patch and then failed must come back as a job about that patch.
    /// Coming back as the issue it started on would begin the work again and orphan the patch.
    #[tokio::test]
    async fn remembers_the_patch_a_job_produced_across_a_restart() {
        let queue = queue().await;
        let patch: Source =
            Subject::Patch(ObjectId::from_str(PRODUCED).expect("patch should parse")).into();
        let id = queue
            .push("answer it".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("job should be recorded");
        let taken = queue.next().await.expect("queue should be readable");
        assert_eq!(taken.source, subject());

        queue
            .retarget(id, patch.clone())
            .await
            .expect("the job should be retargeted");
        assert_eq!(
            queue.running().await.map(|running| running.source),
            Some(patch.clone()),
            "the running job must be about the patch from here on"
        );

        assert_eq!(
            queue.recover().await.expect("queue should be readable"),
            1,
            "a job which never finished must come back"
        );
        let recovered = queue.next().await.expect("queue should be readable");

        assert_eq!(recovered.source, patch);
    }

    #[tokio::test]
    async fn runs_urgent_work_before_older_waiting_work() {
        let queue = queue().await;
        for (brief, position) in [
            ("first", QueuePosition::End),
            ("second", QueuePosition::End),
            ("blocking somebody", QueuePosition::Next),
            ("broken now", QueuePosition::Front),
        ] {
            let _id = queue
                .push(brief.to_owned(), subject(), position)
                .await
                .expect("job should be recorded");
        }

        let mut order = Vec::new();
        while let Some(job) = queue.take().await.expect("queue should be readable") {
            order.push(job.brief);
        }
        assert_eq!(
            order,
            ["broken now", "blocking somebody", "first", "second"],
            "position must decide first and arrival second, so that work nobody marked urgent \
             still runs in the order it was promised"
        );
    }

    #[tokio::test]
    async fn keeps_the_brief_and_its_source_together() {
        let queue = queue().await;
        let _id = queue
            .push(
                "Use Postgres, not MySQL".to_owned(),
                subject(),
                QueuePosition::End,
            )
            .await
            .expect("job should be recorded");

        let pending = queue.pending().await.expect("queue should be readable");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].brief, "Use Postgres, not MySQL");
        assert_eq!(
            pending[0].source,
            subject(),
            "a job is only traceable back to its discussion through its source"
        );

        let taken = queue.take().await.expect("queue should be readable");
        assert!(taken.is_some(), "the recorded job should be takeable");
        assert!(
            queue
                .pending()
                .await
                .expect("queue should be readable")
                .is_empty(),
            "a taken job must not be handed out twice"
        );
    }

    #[tokio::test]
    async fn puts_work_interrupted_by_a_dead_run_back_at_the_front() {
        let queue = queue().await;
        queue
            .push("first".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("first job should be recorded");
        queue
            .push("second".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("second job should be recorded");
        let taken = queue
            .take()
            .await
            .expect("queue should be readable")
            .expect("a job should be takeable");
        assert_eq!(taken.brief, "first");

        // The run dies here, leaving the job marked taken and nothing to finish it.
        assert_eq!(
            queue.recover().await.expect("queue should recover"),
            1,
            "a job left marked taken belonged to a run which died"
        );
        assert_eq!(
            queue
                .take()
                .await
                .expect("queue should be readable")
                .expect("the recovered job should be takeable")
                .brief,
            "first",
            "recovered work must run before anything queued behind it"
        );
    }

    #[tokio::test]
    async fn forgets_work_which_actually_finished() {
        let queue = queue().await;
        queue
            .push("only".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("job should be recorded");
        let _taken = queue.next().await.expect("job should be taken");
        queue.finish().await.expect("job should be removed");

        assert_eq!(
            queue.recover().await.expect("queue should recover"),
            0,
            "a finished job must not come back as interrupted work"
        );
        assert_eq!(
            queue.waiting().await.expect("queue should be readable"),
            0,
            "a finished job must not stay in the queue"
        );
    }

    #[tokio::test]
    async fn waits_for_work_that_has_not_arrived_yet() {
        let queue = queue().await;
        let waiting = tokio::spawn({
            let queue = queue.clone();
            async move { queue.next().await }
        });

        let _id = queue
            .push("write it".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("job should be recorded");

        let job = waiting
            .await
            .expect("waiting task should finish")
            .expect("queue should be readable");
        assert_eq!(job.brief, "write it");
        assert_eq!(
            queue.running().await.map(|running| running.id),
            Some(job.id),
            "a job handed out must be reportable as running while it lasts"
        );

        queue.finish().await.expect("job should be removed");
        assert!(queue.running().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_to_stop_never_outlives_the_job_it_named() {
        let queue = queue().await;
        for brief in ["first", "second"] {
            let _id = queue
                .push(brief.to_owned(), subject(), QueuePosition::End)
                .await
                .expect("job should be recorded");
        }
        let first = queue.next().await.expect("queue should be readable");

        assert_eq!(
            queue.cancel().await.map(|job| job.id),
            Some(first.id),
            "a request to stop reports the job it reached"
        );
        queue.finish().await.expect("job should be removed");
        let second = queue.next().await.expect("queue should be readable");

        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.cancelled(second.id))
                .await
                .is_err(),
            "a request aimed at a job that has already ended must not stop the next one"
        );
        let _cancelled = queue.cancel().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.cancelled(second.id))
                .await
                .is_ok(),
            "a request aimed at the running job must reach it"
        );
    }

    #[tokio::test]
    async fn hands_every_note_to_the_running_job_once() {
        let queue = queue().await;
        let _id = queue
            .push("write it".to_owned(), subject(), QueuePosition::End)
            .await
            .expect("job should be recorded");
        let job = queue.next().await.expect("queue should be readable");

        for note in ["use the existing parser", "skip the migration"] {
            assert_eq!(
                queue.note(note.to_owned()).await.map(|running| running.id),
                Some(job.id)
            );
        }
        assert_eq!(
            queue.notes().await,
            ["use the existing parser", "skip the migration"]
        );
        assert!(
            queue.notes().await.is_empty(),
            "a note already read must not repeat on the next tool call"
        );
    }

    #[tokio::test]
    async fn refuses_a_note_with_nothing_running() {
        let queue = queue().await;

        assert!(
            queue.note("too late".to_owned()).await.is_none(),
            "a note nobody is there to read must be refused, not left for the next job"
        );
        assert!(queue.notes().await.is_empty());
    }
}
