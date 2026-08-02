//! Work queue and handoff between one member's two halves.
//!
//! A member answers messages while it builds, so the two cannot share one train of thought. The
//! talking side records work here instead of performing it, and the building side takes jobs from
//! here instead of acting on whatever the watcher noticed most recently. The queue survives
//! restarts because a job accepted in front of somebody is a promise, and losing it silently is
//! worse than refusing it.
//!
//! Each job carries a brief and the place the request came from. A brief is written by whoever
//! accepted the work, so the builder can start without reconstructing a discussion it never saw.
//!
//! Waiting work is durable, but everything the two halves say to each other while a job runs is
//! not: a note, a request to stop, and which job is running only mean anything to the process
//! holding them. They live here anyway, because they are the same conversation as the queue and
//! splitting them would mean handing both halves two objects instead of one.

use std::{path::Path, str::FromStr as _, sync::Arc};

use radicle::identity::RepoId;
use sqlite::State;

use crate::{database::Database, manifest::QueuePosition, member::Member, watcher::Subject};

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
";

/// One unit of work waiting for a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Database identifier, assigned when the job is recorded.
    pub id: i64,
    /// What to do, in enough detail to begin without rereading the discussion.
    pub brief: String,
    /// Place the request came from.
    pub source: Subject,
    /// Where the job joined the queue.
    pub position: QueuePosition,
}

/// Per-member work queue and the signals its two halves exchange.
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
        source: Subject,
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

    /// Records that no job is running and discards anything said to the finished one.
    pub async fn finish(&self) {
        *self.running.write().await = None;
        self.notes.lock().await.clear();
    }

    /// Returns the job currently being carried out.
    pub async fn running(&self) -> Option<Job> {
        self.running.read().await.clone()
    }

    /// Records a correction for the running job to read on its next tool call.
    pub async fn note(&self, note: String) {
        self.notes.lock().await.push(note);
    }

    /// Takes every note recorded since the last time they were read.
    pub async fn notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().await)
    }

    /// Asks the running job to stop and preserve what it has, returning the job reached.
    ///
    /// The request names the job it was meant for. Asking to stop and the job ending on its own
    /// can happen at the same moment, and a request that outlived its job would otherwise stop the
    /// next one the instant it started.
    pub async fn cancel(&self) -> Option<Job> {
        let running = self.running.read().await.clone();
        if let Some(job) = &running {
            let _previous = self.cancellation.send_replace(Some(job.id));
        }

        running
    }

    /// Waits until somebody asks `job` in particular to stop.
    pub async fn cancelled(&self, job: i64) {
        let mut cancellations = self.cancellation.subscribe();
        while *cancellations.borrow_and_update() != Some(job) {
            if cancellations.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    /// Takes the job that should run next, marking it as taken.
    ///
    /// Jobs are ordered by position first and arrival second, so work promised ahead of the queue
    /// runs before older work that nobody marked urgent.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue cannot be read or updated.
    pub async fn take(&self) -> Result<Option<Job>, Error> {
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
        source: Subject::from_str(&source).map_err(|error| sqlite::Error {
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
        database::Database, manifest::QueuePosition, member::Member, queue::WorkQueue,
        watcher::Subject,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SUBJECT: &str = "1111111111111111111111111111111111111111";

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

    fn subject() -> Subject {
        Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse"))
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

        queue.finish().await;
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
        queue.finish().await;
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
        queue.note("use the existing parser".to_owned()).await;
        queue.note("skip the migration".to_owned()).await;

        assert_eq!(
            queue.notes().await,
            ["use the existing parser", "skip the migration"]
        );
        assert!(
            queue.notes().await.is_empty(),
            "a note already read must not repeat on the next tool call"
        );
    }
}
