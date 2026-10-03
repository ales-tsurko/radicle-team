//! Asynchronous access to the SQLite driver shared with Radicle.
//!
//! Radicle already links `sqlite3` through `sqlite`, and SQLx and rusqlite cannot coexist in the
//! same dependency graph because their `-sys` crates claim that native library separately. This
//! wrapper keeps one driver and moves every database call onto Tokio's blocking pool.

use std::{
    path::Path,
    sync::{Arc, Mutex, PoisonError},
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
