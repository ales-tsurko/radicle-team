//! Asynchronous access to the SQLite driver shared with Radicle.
//!
//! Radicle already links `sqlite3` through `sqlite`, and SQLx and rusqlite cannot coexist in the
//! same dependency graph because their `-sys` crates claim that native library separately. This
//! wrapper keeps one driver and moves every database call onto Tokio's blocking pool.

use std::{
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use sqlite::{Connection, ConnectionThreadSafe, OpenFlags};

#[derive(Clone)]
pub(crate) struct Database(Arc<Mutex<ConnectionThreadSafe>>);

impl Database {
    pub(crate) async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_owned();
        Self::blocking(move || {
            Self::open_connection(path, OpenFlags::new().with_create().with_read_write())
        })
        .await
    }

    pub(crate) async fn read_only(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_owned();
        Self::blocking(move || Self::open_connection(path, OpenFlags::new().with_read_only())).await
    }

    #[cfg(test)]
    pub(crate) async fn memory() -> Result<Self, Error> {
        Self::open(":memory:").await
    }

    pub(crate) async fn execute(&self, statement: &'static str) -> Result<(), Error> {
        self.call(move |connection| connection.execute(statement))
            .await
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&ConnectionThreadSafe) -> sqlite::Result<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.0);
        Self::blocking(move || {
            let connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
            operation(&connection)
        })
        .await
    }

    /// Repeats an atomic or idempotent operation when another connection holds its lock.
    ///
    /// Retrying only the database operation preserves the surrounding delivery's progress.
    /// Async waits release the connection and can be cancelled during process shutdown.
    pub(crate) async fn retry<T: Send + 'static>(
        &self,
        operation: impl Fn(&ConnectionThreadSafe) -> sqlite::Result<T> + Send + Sync + 'static,
    ) -> Result<T, Error> {
        let operation = Arc::new(operation);
        let mut waiting = false;
        loop {
            let attempt = Arc::clone(&operation);
            match self.call(move |connection| attempt(connection)).await {
                Err(Error::Sqlite(error)) if error.code.is_some_and(|code| code & 0xff == 5) => {
                    if !waiting {
                        tracing::warn!(%error, "member state database is busy; waiting for its lock");
                        waiting = true;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                outcome => {
                    if waiting && outcome.is_ok() {
                        tracing::info!("member state database lock cleared");
                    }
                    return outcome;
                }
            }
        }
    }

    fn new(connection: ConnectionThreadSafe) -> Self {
        Self(Arc::new(Mutex::new(connection)))
    }

    pub(crate) fn has_table(connection: &ConnectionThreadSafe, name: &str) -> sqlite::Result<bool> {
        let mut select =
            connection.prepare("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1")?;
        select.bind((1, name))?;
        Ok(select.next()? == sqlite::State::Row)
    }

    fn open_connection(path: impl AsRef<Path>, flags: OpenFlags) -> sqlite::Result<Self> {
        let mut connection = Connection::open_thread_safe_with_flags(path, flags)?;
        connection.set_busy_timeout(5_000)?;
        Ok(Self::new(connection))
    }

    async fn blocking<T, F>(operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce() -> sqlite::Result<T> + Send + 'static,
    {
        tokio::task::spawn_blocking(operation)
            .await
            .map_err(|error| Error::Task(error.to_string()))?
            .map_err(Error::Sqlite)
    }
}

#[derive(Debug, thiserror::Error)]
/// Failure performing an operation on the state database.
pub enum Error {
    /// The SQLite driver failed.
    #[error(transparent)]
    Sqlite(#[from] sqlite::Error),
    /// The blocking database worker failed.
    #[error("SQLite task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use crate::database::{Database, Error};

    #[tokio::test]
    async fn retries_temporary_contention_without_duplicating_a_write() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("state.db");
        let database = Database::open(&path).await.expect("database");
        database
            .execute("CREATE TABLE writes (id INTEGER)")
            .await
            .expect("schema");
        database
            .execute("PRAGMA busy_timeout = 1")
            .await
            .expect("short timeout");
        let writer = Database::open(&path).await.expect("competing connection");
        writer
            .execute("BEGIN IMMEDIATE")
            .await
            .expect("hold write lock");
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempted = attempts.clone();
        let writing = database.retry(move |connection| {
            attempted.fetch_add(1, Ordering::SeqCst);
            connection.execute("INSERT INTO writes VALUES (1)")
        });
        let release = async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while attempts.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("a lock timeout must be retried without blocking async work");
            writer.execute("COMMIT").await.expect("release write lock");
        };
        let (written, ()) = tokio::join!(writing, release);
        written.expect("write eventually succeeds");
        let count = database
            .call(|connection| {
                let mut select = connection.prepare("SELECT count(*) FROM writes")?;
                select.next()?;
                select.read::<i64, _>(0)
            })
            .await
            .expect("stored writes");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn permanent_errors_are_returned_and_lock_retries_can_be_cancelled() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("state.db");
        let database = Database::open(&path).await.expect("database");
        database
            .execute("CREATE TABLE writes (id INTEGER)")
            .await
            .expect("schema");
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            database.retry(|connection| connection.execute("INSERT INTO missing VALUES (1)")),
        )
        .await
        .expect("invalid SQL must not be retried")
        .expect_err("missing table");
        assert!(matches!(error, Error::Sqlite(error) if error.code == Some(1)));
        database
            .execute("PRAGMA busy_timeout = 1")
            .await
            .expect("short timeout");
        let writer = Database::open(&path).await.expect("competing connection");
        writer
            .execute("BEGIN EXCLUSIVE")
            .await
            .expect("hold database lock");
        let reading = database.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempted = attempts.clone();
        let waiting = tokio::spawn(async move {
            reading
                .retry(move |connection| {
                    attempted.fetch_add(1, Ordering::SeqCst);
                    let mut select = connection.prepare("SELECT id FROM writes")?;
                    select.next().map(|_| ())
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("read lock contention must be retried");
        waiting.abort();
        assert!(waiting.await.expect_err("cancelled retry").is_cancelled());
        writer
            .execute("COMMIT")
            .await
            .expect("release database lock");
        database
            .execute("INSERT INTO writes VALUES (1)")
            .await
            .expect("connection remains usable");
    }
}
