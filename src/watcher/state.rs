//! Persistent activity-delivery deduplication for watcher rescans.

use std::path::Path;

use radicle::{
    cob::{EntryId, ObjectId},
    identity::RepoId,
};
use sqlite::State;

use crate::{database::Database, watcher::Error};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS processed_activities (
        repository TEXT NOT NULL,
        object TEXT NOT NULL,
        operation TEXT NOT NULL,
        action TEXT NOT NULL,
        PRIMARY KEY (repository, object, operation, action)
    ) STRICT;
";

pub(super) struct ProcessedActivities {
    database: Database,
}

impl ProcessedActivities {
    pub(super) async fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Self::from_database(Database::open(path).await?).await
    }

    #[cfg(all(test, not(miri)))]
    pub(super) async fn memory() -> Result<Self, Error> {
        Self::from_database(Database::memory().await?).await
    }

    async fn from_database(database: Database) -> Result<Self, Error> {
        database.execute(SCHEMA).await?;
        Ok(Self { database })
    }

    pub(super) async fn contains(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<bool, Error> {
        self.operation(repository, object, operation, action, Operation::Contains)
            .await
    }

    pub(super) async fn insert(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<bool, Error> {
        self.operation(repository, object, operation, action, Operation::Insert)
            .await
    }

    async fn operation(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
        kind: Operation,
    ) -> Result<bool, Error> {
        let values = operation_values(repository, object, operation, action);
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(kind.query())?;
                bind_operation(&mut statement, &values)?;
                match kind {
                    Operation::Contains => Ok(statement.next()? == State::Row),
                    Operation::Insert => {
                        statement.next()?;
                        Ok(connection.change_count() == 1)
                    }
                }
            })
            .await
            .map_err(Error::from)
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Contains,
    Insert,
}

impl Operation {
    const fn query(self) -> &'static str {
        match self {
            Self::Contains => {
                "SELECT 1 FROM processed_activities
                 WHERE repository = ?1 AND object = ?2 AND operation = ?3 AND action = ?4"
            }
            Self::Insert => {
                "INSERT OR IGNORE INTO processed_activities
                     (repository, object, operation, action)
                 VALUES (?1, ?2, ?3, ?4)"
            }
        }
    }
}

fn operation_values(
    repository: RepoId,
    object: ObjectId,
    operation: EntryId,
    action: usize,
) -> [String; 4] {
    [
        repository.to_string(),
        object.to_string(),
        operation.to_string(),
        action.to_string(),
    ]
}

fn bind_operation(
    statement: &mut sqlite::Statement<'_>,
    values: &[String; 4],
) -> sqlite::Result<()> {
    for (index, value) in values.iter().enumerate() {
        statement.bind((index + 1, value.as_str()))?;
    }
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        cob::{EntryId, ObjectId},
        identity::RepoId,
    };

    use crate::watcher::state::ProcessedActivities;

    #[tokio::test]
    async fn tracks_each_action_once() {
        let state = ProcessedActivities::memory()
            .await
            .expect("state should open");
        let repository =
            RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("valid repository");
        let object =
            ObjectId::from_str("1111111111111111111111111111111111111111").expect("valid object");
        let operation =
            EntryId::from_str("2222222222222222222222222222222222222222").expect("valid operation");

        assert!(
            !state
                .contains(repository, object, operation, 0)
                .await
                .expect("contains should work")
        );
        assert!(
            state
                .insert(repository, object, operation, 0)
                .await
                .expect("insert should work")
        );
        assert!(
            !state
                .insert(repository, object, operation, 0)
                .await
                .expect("duplicate should work")
        );
        assert!(
            state
                .contains(repository, object, operation, 0)
                .await
                .expect("contains should work")
        );
        assert!(
            !state
                .contains(repository, object, operation, 1)
                .await
                .expect("contains should work")
        );
    }
}
