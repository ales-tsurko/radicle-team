//! Persistent activity-delivery deduplication for watcher rescans.

use std::path::Path;

use radicle::{
    cob::{EntryId, ObjectId},
    identity::RepoId,
};
use sqlite::{ConnectionThreadSafe, State};

use crate::{agent::LegacyPendingReply, database::Database, watcher::Error};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS processed_operations (
        repository TEXT NOT NULL,
        object TEXT NOT NULL,
        operation TEXT NOT NULL,
        PRIMARY KEY (repository, object, operation)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS processed_activities (
        repository TEXT NOT NULL,
        object TEXT NOT NULL,
        operation TEXT NOT NULL,
        action TEXT NOT NULL,
        PRIMARY KEY (repository, object, operation, action)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS pending_replies (
        repository TEXT NOT NULL,
        object TEXT NOT NULL,
        operation TEXT NOT NULL,
        action TEXT NOT NULL,
        reply TEXT NOT NULL,
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

    pub(super) async fn pending_reply(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<Option<LegacyPendingReply>, Error> {
        let values = operation_values(repository, object, operation, action);
        let reply = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT reply FROM pending_replies
                     WHERE repository = ?1 AND object = ?2
                       AND operation = ?3 AND action = ?4",
                )?;
                bind_operation(&mut statement, &values)?;
                match statement.next()? {
                    State::Row => statement.read::<String, _>(0).map(Some),
                    State::Done => Ok(None),
                }
            })
            .await?;
        reply
            .map(|reply| serde_json::from_str(&reply))
            .transpose()
            .map_err(|error| Error::Database(error.to_string()))
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
                        drop(statement);
                        Self::checkpoint(connection, &values)
                    }
                }
            })
            .await
            .map_err(Error::from)
    }

    fn checkpoint(connection: &ConnectionThreadSafe, values: &[String; 4]) -> sqlite::Result<bool> {
        connection.execute("BEGIN IMMEDIATE")?;
        Self::checkpoint_transaction(connection, values)
            .and_then(|inserted| {
                connection.execute("COMMIT")?;
                Ok(inserted)
            })
            .inspect_err(|_| {
                let _rollback = connection.execute("ROLLBACK");
            })
    }

    fn checkpoint_transaction(
        connection: &ConnectionThreadSafe,
        values: &[String; 4],
    ) -> sqlite::Result<bool> {
        let mut insert = connection.prepare(Operation::Insert.query())?;
        bind_operation(&mut insert, values)?;
        insert.next()?;
        let inserted = connection.change_count() == 1;
        drop(insert);
        let mut clear = connection.prepare(
            "DELETE FROM pending_replies
             WHERE repository = ?1 AND object = ?2 AND operation = ?3 AND action = ?4",
        )?;
        bind_operation(&mut clear, values)?;
        clear.next()?;
        Ok(inserted)
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
                "SELECT 1
                 WHERE EXISTS (
                     SELECT 1 FROM processed_operations
                     WHERE repository = ?1 AND object = ?2 AND operation = ?3
                 ) OR EXISTS (
                     SELECT 1 FROM processed_activities
                     WHERE repository = ?1 AND object = ?2 AND operation = ?3 AND action = ?4
                 )"
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
        crypto::PublicKey,
        identity::RepoId,
    };

    use crate::{
        agent::{LegacyPendingReply, LegacyReply, LegacyReplyTarget},
        database::Database,
        watcher::state::{ProcessedActivities, bind_operation, operation_values},
    };

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

    #[tokio::test]
    async fn loads_and_clears_legacy_pending_replies_on_checkpoint() {
        let repository =
            RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc").expect("valid repository");
        let object =
            ObjectId::from_str("1111111111111111111111111111111111111111").expect("valid object");
        let operation =
            EntryId::from_str("2222222222222222222222222222222222222222").expect("valid operation");
        let pending = LegacyPendingReply {
            reply: LegacyReply {
                body: "Recovered reply".to_owned(),
                target: LegacyReplyTarget::Issue { reply_to: None },
            },
            author: PublicKey::from_str("z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7")
                .expect("valid key"),
        };
        let values = operation_values(repository, object, operation, 0);
        let encoded = serde_json::to_string(&pending).expect("pending reply should serialize");
        let database = Database::memory().await.expect("database should open");
        database
            .execute(
                "CREATE TABLE pending_replies (
                    repository TEXT NOT NULL,
                    object TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    action TEXT NOT NULL,
                    reply TEXT NOT NULL,
                    PRIMARY KEY (repository, object, operation, action)
                ) STRICT;",
            )
            .await
            .expect("legacy schema should initialize");
        database
            .call(move |connection| {
                let mut insert = connection.prepare(
                    "INSERT INTO pending_replies
                         (repository, object, operation, action, reply)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                bind_operation(&mut insert, &values)?;
                insert.bind((5, encoded.as_str()))?;
                insert.next()?;
                Ok(())
            })
            .await
            .expect("legacy row should insert");
        let state = ProcessedActivities::from_database(database)
            .await
            .expect("upgraded state should open");

        assert_eq!(
            state
                .pending_reply(repository, object, operation, 0)
                .await
                .expect("pending reply should load"),
            Some(pending)
        );
        assert!(
            state
                .insert(repository, object, operation, 0)
                .await
                .expect("checkpoint should insert")
        );
        assert_eq!(
            state
                .pending_reply(repository, object, operation, 0)
                .await
                .expect("pending reply lookup should work"),
            None
        );
    }
}
