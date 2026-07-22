//! Persistent activity-delivery deduplication for idempotent watcher rescans.

use std::path::Path;

use radicle::{
    cob::{EntryId, ObjectId},
    crypto::PublicKey,
    identity::RepoId,
};
use sqlite::{ConnectionThreadSafe, State};

use crate::{agent::Reply, database::Database, watcher::Error};

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingReply {
    pub(super) reply: Reply,
    pub(super) author: PublicKey,
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
        self.flag_operation(repository, object, operation, action, Operation::Contains)
            .await
    }

    pub(super) async fn insert(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<bool, Error> {
        self.flag_operation(repository, object, operation, action, Operation::Insert)
            .await
    }

    async fn flag_operation(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
        kind: Operation,
    ) -> Result<bool, Error> {
        match self
            .operation(repository, object, operation, action, kind)
            .await?
        {
            OperationResult::Flag(value) => Ok(value),
            OperationResult::Reply(_) => Err(Self::invalid_operation()),
        }
    }

    pub(super) async fn pending_reply(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<Option<PendingReply>, Error> {
        let reply = match self
            .operation(repository, object, operation, action, Operation::LoadReply)
            .await?
        {
            OperationResult::Reply(reply) => reply,
            OperationResult::Flag(_) => return Err(Self::invalid_operation()),
        };
        reply
            .map(|reply| serde_json::from_str(&reply))
            .transpose()
            .map_err(|error| Error::ReplyState(error.to_string()))
    }

    pub(super) async fn store_reply(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
        reply: &Reply,
        author: PublicKey,
    ) -> Result<(), Error> {
        let values = operation_values(repository, object, operation, action);
        let reply = serde_json::to_string(&PendingReply {
            reply: reply.clone(),
            author,
        })
        .map_err(|error| Error::ReplyState(error.to_string()))?;
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "INSERT OR REPLACE INTO pending_replies
                         (repository, object, operation, action, reply)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                bind_operation(&mut statement, &values)?;
                statement.bind((5, reply.as_str()))?;
                statement.next()?;
                Ok(())
            })
            .await
            .map_err(Error::from)
    }

    pub(super) async fn clear_reply(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
    ) -> Result<(), Error> {
        let _cleared = self
            .operation(repository, object, operation, action, Operation::ClearReply)
            .await?;
        Ok(())
    }

    async fn operation(
        &self,
        repository: RepoId,
        object: ObjectId,
        operation: EntryId,
        action: usize,
        kind: Operation,
    ) -> Result<OperationResult, Error> {
        let values = operation_values(repository, object, operation, action);
        self.database
            .call(move |connection| match kind {
                Operation::Contains => {
                    let mut statement = connection.prepare(kind.query())?;
                    bind_operation(&mut statement, &values)?;
                    Ok(OperationResult::Flag(statement.next()? == State::Row))
                }
                Operation::Insert => {
                    Self::checkpoint(connection, &values).map(OperationResult::Flag)
                }
                Operation::LoadReply => {
                    let mut statement = connection.prepare(kind.query())?;
                    bind_operation(&mut statement, &values)?;
                    match statement.next()? {
                        State::Row => statement
                            .read::<String, _>(0)
                            .map(Some)
                            .map(OperationResult::Reply),
                        State::Done => Ok(OperationResult::Reply(None)),
                    }
                }
                Operation::ClearReply => {
                    let mut statement = connection.prepare(kind.query())?;
                    bind_operation(&mut statement, &values)?;
                    statement.next()?;
                    Ok(OperationResult::Reply(None))
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
        let mut clear = connection.prepare(Operation::ClearReply.query())?;
        bind_operation(&mut clear, values)?;
        clear.next()?;
        Ok(inserted)
    }

    fn invalid_operation() -> Error {
        Error::ReplyState("invalid internal watcher state operation".to_owned())
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Contains,
    Insert,
    LoadReply,
    ClearReply,
}

enum OperationResult {
    Flag(bool),
    Reply(Option<String>),
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
            Self::LoadReply => {
                "SELECT reply FROM pending_replies
                 WHERE repository = ?1 AND object = ?2 AND operation = ?3 AND action = ?4"
            }
            Self::ClearReply => {
                "DELETE FROM pending_replies
                 WHERE repository = ?1 AND object = ?2 AND operation = ?3 AND action = ?4"
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
        agent::{Reply, ReplyTarget},
        watcher::state::{PendingReply, ProcessedActivities},
    };

    #[tokio::test]
    async fn distinguishes_actions_within_an_operation() {
        let store = ProcessedActivities::memory()
            .await
            .expect("in-memory store should open");
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let object = ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("object ID should be valid");
        let operation = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation ID should be valid");

        assert!(
            !store
                .contains(repository, object, operation, 0)
                .await
                .expect("lookup")
        );
        assert!(
            store
                .insert(repository, object, operation, 0)
                .await
                .expect("insert")
        );
        assert!(
            !store
                .insert(repository, object, operation, 0)
                .await
                .expect("duplicate")
        );
        assert!(
            store
                .contains(repository, object, operation, 0)
                .await
                .expect("lookup")
        );
        assert!(
            !store
                .contains(repository, object, operation, 1)
                .await
                .expect("distinct action lookup")
        );
        assert!(
            store
                .insert(repository, object, operation, 1)
                .await
                .expect("distinct action insert")
        );
    }

    #[tokio::test]
    async fn persists_pending_replies_until_delivery_completes() {
        let store = ProcessedActivities::memory()
            .await
            .expect("in-memory store should open");
        let repository = RepoId::from_str("rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc")
            .expect("repository ID should be valid");
        let object = ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("object ID should be valid");
        let operation = EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation ID should be valid");
        let reply = Reply {
            body: "Reply".to_owned(),
            target: ReplyTarget::Issue { reply_to: None },
        };
        let author = PublicKey::from_str("z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7")
            .expect("author should be valid");

        store
            .store_reply(repository, object, operation, 0, &reply, author)
            .await
            .expect("reply should be stored");
        assert_eq!(
            store
                .pending_reply(repository, object, operation, 0)
                .await
                .expect("reply should load"),
            Some(PendingReply {
                reply: reply.clone(),
                author
            })
        );
        store
            .database
            .execute(
                "CREATE TRIGGER reject_pending_reply_cleanup
                 BEFORE DELETE ON pending_replies
                 BEGIN
                     SELECT RAISE(FAIL, 'cleanup rejected');
                 END",
            )
            .await
            .expect("failure trigger should be installed");
        store
            .insert(repository, object, operation, 0)
            .await
            .expect_err("checkpoint cleanup should fail");
        assert!(
            !store
                .contains(repository, object, operation, 0)
                .await
                .expect("failed checkpoint should roll back")
        );
        assert_eq!(
            store
                .pending_reply(repository, object, operation, 0)
                .await
                .expect("reply should survive a failed checkpoint"),
            Some(PendingReply {
                reply: reply.clone(),
                author
            })
        );
        store
            .database
            .execute("DROP TRIGGER reject_pending_reply_cleanup")
            .await
            .expect("failure trigger should be removed");
        store
            .insert(repository, object, operation, 0)
            .await
            .expect("delivery should complete");
        assert_eq!(
            store
                .pending_reply(repository, object, operation, 0)
                .await
                .expect("reply lookup should succeed"),
            None
        );
    }
}
