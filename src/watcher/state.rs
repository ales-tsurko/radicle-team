//! Persistent operation deduplication for idempotent watcher rescans.

use std::{fs, path::Path};

use radicle::{
    cob::{EntryId, ObjectId},
    identity::RepoId,
};
#[cfg(all(test, not(miri)))]
use sqlite::State;
use sqlite::{Connection, Statement};

use crate::watcher::Error;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS processed_operations (
        repository TEXT NOT NULL,
        object TEXT NOT NULL,
        operation TEXT NOT NULL,
        PRIMARY KEY (repository, object, operation)
    ) STRICT;
";

pub(super) struct ProcessedOperations {
    connection: Connection,
}

impl ProcessedOperations {
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    #[cfg(all(test, not(miri)))]
    fn memory() -> Result<Self, Error> {
        Self::from_connection(Connection::open(":memory:")?)
    }

    fn from_connection(connection: Connection) -> Result<Self, Error> {
        connection.execute(SCHEMA)?;
        Ok(Self { connection })
    }

    #[cfg(all(test, not(miri)))]
    fn contains(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
    ) -> Result<bool, Error> {
        let mut statement = self.operation_statement(
            "SELECT 1 FROM processed_operations
             WHERE repository = ?1 AND object = ?2 AND operation = ?3",
            repository,
            object,
            operation,
        )?;

        Ok(statement.next()? == State::Row)
    }

    pub(super) fn insert(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
    ) -> Result<bool, Error> {
        let mut statement = self.operation_statement(
            "INSERT OR IGNORE INTO processed_operations (repository, object, operation)
             VALUES (?1, ?2, ?3)",
            repository,
            object,
            operation,
        )?;
        statement.next()?;

        Ok(self.connection.change_count() == 1)
    }

    fn operation_statement<'connection>(
        &'connection self,
        query: &str,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
    ) -> Result<Statement<'connection>, Error> {
        let mut statement = self.connection.prepare(query)?;
        let repository = repository.to_string();
        let object = object.to_string();
        let operation = operation.to_string();
        statement.bind((1, repository.as_str()))?;
        statement.bind((2, object.as_str()))?;
        statement.bind((3, operation.as_str()))?;

        Ok(statement)
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        cob::{EntryId, ObjectId},
        identity::RepoId,
    };

    use crate::watcher::state::ProcessedOperations;

    #[test]
    fn remembers_processed_operations() {
        let store = ProcessedOperations::memory().expect("in-memory store should open");
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let object = ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("object ID should be valid");
        let operation = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation ID should be valid");

        assert!(
            !store
                .contains(repository, object, operation)
                .expect("lookup should succeed")
        );
        assert!(
            store
                .insert(repository, object, operation)
                .expect("insert should succeed")
        );
        assert!(
            !store
                .insert(repository, object, operation)
                .expect("duplicate insert should succeed")
        );
        assert!(
            store
                .contains(repository, object, operation)
                .expect("lookup should succeed")
        );
    }
}
