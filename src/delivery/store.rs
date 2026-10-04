//! SQLite inbox. Admission finishes before the model starts and survives process exit.

use std::{sync::Arc, time::Duration};

use sqlite::State as Row;
use tokio::sync::Notify;

use crate::{
    database::Database,
    delivery::{Error, Record, State},
    status::Snapshot,
    watcher::Activity,
};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS delivery_inbox (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE, data TEXT NOT NULL, priority INTEGER NOT NULL,
    status TEXT NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0,
    receipt INTEGER NOT NULL DEFAULT 0, published INTEGER NOT NULL DEFAULT 0
) STRICT;";

#[derive(Clone)]
pub(crate) struct Inbox {
    database: Database,
    changed: Arc<Notify>,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").finish_non_exhaustive()
    }
}

impl Inbox {
    #[cfg(test)]
    pub(crate) async fn open(path: &std::path::Path) -> Result<Self, Error> {
        Self::new(Database::open(path).await?).await
    }

    #[cfg(test)]
    pub(crate) async fn memory() -> Result<Self, Error> {
        Self::new(Database::memory().await?).await
    }

    pub(crate) async fn new(database: Database) -> Result<Self, Error> {
        database.execute(SCHEMA).await?;
        let inbox = Self {
            database,
            changed: Arc::new(Notify::new()),
        };
        inbox.recover().await?;
        Ok(inbox)
    }

    pub(crate) async fn enqueue(&self, activity: Activity, task: Option<i64>) -> Result<(), Error> {
        self.insert(Record::new(activity, task)).await
    }

    pub(crate) async fn ignore(&self, activity: Activity, reason: String) -> Result<(), Error> {
        let mut record = Record::new(activity, None);
        record.state = State::Ignored;
        record.error = Some(reason);
        self.insert(record).await
    }

    async fn insert(&self, record: Record) -> Result<(), Error> {
        self.database
            .retry(move |connection| Self::insert_row(connection, &record))
            .await?;
        self.wake();
        Ok(())
    }

    pub(crate) fn insert_row(
        connection: &sqlite::ConnectionThreadSafe,
        record: &Record,
    ) -> sqlite::Result<()> {
        let data = serde_json::to_string(record).map_err(|error| sqlite::Error {
            code: None,
            message: Some(error.to_string()),
        })?;
        let mut insert = connection.prepare(
            "INSERT OR IGNORE INTO delivery_inbox
            (id, data, priority, status) VALUES (?1, ?2, ?3, ?4)",
        )?;
        insert.bind((1, record.id().as_str()))?;
        insert.bind((2, data.as_str()))?;
        insert.bind((3, record.priority()))?;
        insert.bind((4, record.state.to_string().as_str()))?;
        insert.next()?;
        Ok(())
    }

    pub(crate) fn wake(&self) {
        self.changed.notify_waiters();
    }

    pub(crate) fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }

    pub(crate) async fn recover(&self) -> Result<(), Error> {
        for record in self.list().await? {
            if record.state == State::Processing {
                self.set(&record.id(), State::Queued, None).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn next(&self) -> Result<Record, Error> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let now = Snapshot::now() as i64;
            let data = self
                .database
                .retry(move |connection| {
                    let mut select = connection.prepare(
                        "SELECT data FROM delivery_inbox
                    WHERE status IN ('queued', 'failed') AND retry_at <= ?1
                    AND json_extract(data, '$.task') IS NULL
                    ORDER BY priority, sequence LIMIT 1",
                    )?;
                    select.bind((1, now))?;
                    if select.next()? == Row::Row {
                        Ok(Some(select.read::<String, _>(0)?))
                    } else {
                        Ok(None)
                    }
                })
                .await?;
            if let Some(data) = data {
                let record: Record = serde_json::from_str(&data)?;
                self.set(&record.id(), State::Processing, None).await?;
                return Ok(record);
            }
            tokio::select! { () = notified => {}, () = tokio::time::sleep(Duration::from_secs(1)) => {} }
        }
    }

    pub(crate) async fn set(
        &self,
        id: &str,
        state: State,
        error: Option<String>,
    ) -> Result<(), Error> {
        let id = id.to_owned();
        let status = state.to_string();
        self.database
            .retry(move |connection| {
                let now = Snapshot::now() as i64;
                let retry = if state == State::Failed { now + 10 } else { 0 };
                // One statement avoids upgrading an open read cursor to a write lock.
                let mut update = connection.prepare(
                    "UPDATE delivery_inbox SET
                        data = json_set(data, '$.state', ?2, '$.updated', ?3,
                            '$.error', ?4, '$.revision', json_extract(data, '$.revision') + 1),
                        status = ?2, retry_at = ?5
                     WHERE id = ?1 AND (status IS NOT ?2 OR json_extract(data, '$.error') IS NOT \
                     ?4)",
                )?;
                update.bind((1, id.as_str()))?;
                update.bind((2, status.as_str()))?;
                update.bind((3, now))?;
                update.bind((4, error.as_deref()))?;
                update.bind((5, retry))?;
                update.next()?;
                Ok(())
            })
            .await?;
        self.wake();
        Ok(())
    }

    pub(crate) fn retire_task(
        connection: &sqlite::ConnectionThreadSafe,
        task: i64,
        cancelled: bool,
    ) -> sqlite::Result<()> {
        for data in Self::rows(connection)? {
            let mut record: Record =
                serde_json::from_str(&data).map_err(|error| sqlite::Error {
                    code: None,
                    message: Some(error.to_string()),
                })?;
            if record.task != Some(task) {
                continue;
            }
            record.state = if cancelled {
                State::Ignored
            } else {
                State::Handled
            };
            record.error = cancelled.then(|| "Implementation task cancelled".to_owned());
            record.updated = Snapshot::now();
            record.revision += 1;
            let data = serde_json::to_string(&record).map_err(|error| sqlite::Error {
                code: None,
                message: Some(error.to_string()),
            })?;
            let mut update = connection.prepare(
                "UPDATE delivery_inbox SET data = ?1,
                status = ?2, retry_at = 0 WHERE id = ?3",
            )?;
            update.bind((1, data.as_str()))?;
            update.bind((2, record.state.to_string().as_str()))?;
            update.bind((3, record.id().as_str()))?;
            update.next()?;
        }
        Ok(())
    }

    pub(crate) async fn task(
        &self,
        task: i64,
        state: State,
        error: Option<String>,
    ) -> Result<(), Error> {
        for record in self.list().await? {
            if record.task == Some(task) {
                self.set(&record.id(), state, error.clone()).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn list(&self) -> Result<Vec<Record>, Error> {
        Self::inspect(&self.database).await
    }

    pub(crate) async fn inspect(database: &Database) -> Result<Vec<Record>, Error> {
        let rows = database.retry(Self::rows).await?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Error::from))
            .collect()
    }

    pub(crate) fn rows(connection: &sqlite::ConnectionThreadSafe) -> sqlite::Result<Vec<String>> {
        if !Database::has_table(connection, "delivery_inbox")? {
            return Ok(Vec::new());
        }
        let mut select = connection.prepare("SELECT data FROM delivery_inbox ORDER BY sequence")?;
        let mut data = Vec::new();
        while select.next()? == Row::Row {
            data.push(select.read::<String, _>(0)?);
        }
        Ok(data)
    }

    pub(crate) async fn pending(&self) -> Result<Vec<(Record, bool)>, Error> {
        let rows = self
            .database
            .retry(|connection| {
                let mut select = connection.prepare(
                    "SELECT data, receipt FROM delivery_inbox
                WHERE published < json_extract(data, '$.revision') OR
                (receipt = 0 AND status != 'ignored') ORDER BY sequence",
                )?;
                let mut data = Vec::new();
                while select.next()? == Row::Row {
                    data.push((select.read::<String, _>(0)?, select.read::<i64, _>(1)? != 0));
                }
                Ok(data)
            })
            .await?;
        rows.into_iter()
            .map(|(data, receipt)| Ok((serde_json::from_str(&data)?, receipt)))
            .collect()
    }

    pub(crate) async fn published(&self, record: &Record, receipt: bool) -> Result<(), Error> {
        let id = record.id();
        let revision = record.revision;
        self.database
            .retry(move |connection| {
                let mut update = connection.prepare(
                    "UPDATE delivery_inbox
                SET published = max(published, ?1), receipt = max(receipt, ?2) WHERE id = ?3",
                )?;
                update.bind((1, revision))?;
                update.bind((2, i64::from(receipt)))?;
                update.bind((3, id.as_str()))?;
                update.next()?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        delivery::{Inbox, State},
        watcher::{Activity, Delivery, Kind, Subject, Target},
    };

    fn activity(operation: &str, kind: Kind) -> Activity {
        Activity {
            repository: "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
            subject: Subject::Issue(
                "1111111111111111111111111111111111111111"
                    .parse()
                    .expect("issue"),
            ),
            operation: operation.parse().expect("operation"),
            action: 0,
            author: "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
                .parse()
                .expect("DID"),
            kind,
        }
    }

    #[tokio::test]
    async fn preserves_admission_priority_and_unfinished_work_across_restart() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("state.db");
        let inbox = Inbox::open(&path).await.expect("inbox");
        let ambient = activity(
            "2222222222222222222222222222222222222222",
            Kind::Delivered(Delivery::Changed(crate::watcher::Change::Edited)),
        );
        let direct = activity(
            "3333333333333333333333333333333333333333",
            Kind::Delivered(Delivery::Mentioned(Target::Subject)),
        );
        inbox
            .enqueue(ambient.clone(), None)
            .await
            .expect("ambient admission");
        inbox
            .enqueue(direct.clone(), None)
            .await
            .expect("direct admission");
        inbox
            .enqueue(direct.clone(), None)
            .await
            .expect("idempotent admission");
        assert_eq!(inbox.next().await.expect("next").activity, direct);
        drop(inbox);
        let inbox = Inbox::open(&path).await.expect("reopened");
        inbox.recover().await.expect("recover active delivery");
        let retried = inbox.next().await.expect("retry");
        assert_eq!(retried.activity, direct);
        inbox
            .set(&retried.id(), State::Handled, None)
            .await
            .expect("complete");
        assert_eq!(inbox.next().await.expect("ambient next").activity, ambient);
        inbox.enqueue(direct, None).await.expect("later rescan");
        assert_eq!(inbox.list().await.expect("records").len(), 2);
    }

    #[tokio::test]
    async fn publishing_an_older_snapshot_cannot_clear_a_newer_outbox_update() {
        let inbox = Inbox::memory().await.expect("inbox");
        inbox
            .enqueue(
                activity(
                    "2222222222222222222222222222222222222222",
                    Kind::Delivered(Delivery::Mentioned(Target::Subject)),
                ),
                None,
            )
            .await
            .expect("admit");
        let queued = inbox.list().await.expect("records").remove(0);
        inbox
            .set(&queued.id(), State::Processing, None)
            .await
            .expect("start");
        inbox
            .published(&queued, true)
            .await
            .expect("delayed publication");
        let pending = inbox.pending().await.expect("outbox");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.state, State::Processing);
        assert!(
            pending[0].1,
            "the receipt remains recorded while its status update waits"
        );
    }

    #[tokio::test]
    async fn delivery_transitions_wait_for_a_competing_writer() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("state.db");
        let inbox = Inbox::open(&path).await.expect("inbox");
        inbox
            .enqueue(
                activity(
                    "2222222222222222222222222222222222222222",
                    Kind::Delivered(Delivery::Mentioned(Target::Subject)),
                ),
                None,
            )
            .await
            .expect("admit");
        let record = inbox.list().await.expect("records").remove(0);
        let id = record.id();
        let writer = crate::database::Database::open(&path)
            .await
            .expect("writer");
        writer.execute("BEGIN IMMEDIATE").await.expect("write lock");
        let release = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            writer.execute("COMMIT").await.expect("release write lock");
        };
        let (updated, ()) = tokio::join!(inbox.set(&id, State::Processing, None), release);
        updated.expect("contention must delay the transition rather than fail it");
        let stored = inbox.list().await.expect("records").remove(0);
        assert_eq!(stored.state, State::Processing);
        assert_eq!(stored.revision, record.revision + 1);
    }
}
