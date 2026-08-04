//! Persistent identities for received IRC messages.

#![cfg_attr(
    any(not(test), miri),
    expect(
        dead_code,
        reason = "chat persistence is isolated behind one complete crate-private boundary"
    )
)]

use std::{path::Path, str::FromStr as _};

use radicle::identity::{Did, RepoId};
use sqlite::State as SqliteState;

use crate::{
    chat::{ChannelId, MessageId},
    database::Database,
};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS chat_messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repository TEXT NOT NULL,
        server TEXT NOT NULL,
        channel TEXT NOT NULL,
        irc_message TEXT,
        account TEXT NOT NULL,
        author TEXT NOT NULL,
        handled INTEGER NOT NULL DEFAULT 0 CHECK (handled IN (0, 1)),
        received INTEGER NOT NULL DEFAULT (unixepoch()),
        UNIQUE (repository, server, channel, irc_message)
    ) STRICT;

    CREATE TABLE IF NOT EXISTS chat_outbox (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repository TEXT NOT NULL,
        channel TEXT NOT NULL,
        in_reply_to INTEGER NOT NULL,
        body TEXT NOT NULL CHECK (length(body) > 0),
        sent INTEGER NOT NULL DEFAULT 0 CHECK (sent IN (0, 1)),
        created INTEGER NOT NULL DEFAULT (unixepoch()),
        sent_at INTEGER
    ) STRICT;

    CREATE INDEX IF NOT EXISTS chat_outbox_pending
        ON chat_outbox (repository, id) WHERE sent = 0;
";

/// Whether a message is live or replayed from server history.
///
/// History has no variant without a stable IRC message ID, so it cannot enter delivery without
/// one. A live message may lack the optional `msgid` capability and receives only a local ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receipt<'a> {
    Live { message: Option<&'a str> },
    History { message: &'a str },
}

/// Durable delivery state for a received message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageStatus {
    /// New or previously interrupted delivery which must be attempted.
    Pending(MessageId),
    /// Delivery already completed and must not run again.
    Handled(MessageId),
}

/// Durable local identifier for one queued chat reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OutboxId(i64);

impl OutboxId {
    pub(crate) const fn get(self) -> i64 {
        self.0
    }
}

/// One reply waiting to be sent by the IRC connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutboxMessage {
    pub(crate) id: OutboxId,
    pub(crate) channel: ChannelId,
    pub(crate) in_reply_to: MessageId,
    pub(crate) body: String,
}

impl MessageStatus {
    pub(crate) const fn id(self) -> MessageId {
        match self {
            Self::Pending(id) | Self::Handled(id) => id,
        }
    }
}

/// Durable chat delivery state stored beside watcher and work-queue state.
#[derive(Clone)]
pub(crate) struct State {
    database: Database,
}

impl State {
    /// Creates or upgrades the chat delivery schema.
    pub(crate) async fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let database = Database::open(path).await?;
        database.execute(SCHEMA).await?;
        Ok(Self { database })
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) async fn memory() -> Result<Self, Error> {
        let database = Database::memory().await?;
        database.execute(SCHEMA).await?;
        Ok(Self { database })
    }

    /// Records a received message and returns whether it still needs delivery.
    pub(crate) async fn record(
        &self,
        repository: RepoId,
        server: &str,
        channel: &ChannelId,
        receipt: Receipt<'_>,
        account: &str,
        author: Did,
    ) -> Result<MessageStatus, Error> {
        let network_message = match receipt {
            Receipt::Live { message } => message,
            Receipt::History { message } => Some(message),
        };
        if network_message.is_some_and(str::is_empty) {
            return Err(Error::InvalidNetworkMessageId);
        }

        let repository = repository.to_string();
        let server = server.to_owned();
        let channel = channel.to_string();
        let network_message = network_message.map(str::to_owned);
        let account = account.to_owned();
        let author = author.to_string();
        self.database
            .call(move |connection| {
                if let Some(network_message) = &network_message {
                    let mut existing = connection.prepare(
                        "SELECT id, handled FROM chat_messages
                         WHERE repository = ?1 AND server = ?2 AND channel = ?3
                           AND irc_message = ?4",
                    )?;
                    existing.bind((1, repository.as_str()))?;
                    existing.bind((2, server.as_str()))?;
                    existing.bind((3, channel.as_str()))?;
                    existing.bind((4, network_message.as_str()))?;
                    if existing.next()? == SqliteState::Row {
                        let id = message_id(existing.read::<i64, _>("id")?)?;
                        return if existing.read::<i64, _>("handled")? == 0 {
                            Ok(MessageStatus::Pending(id))
                        } else {
                            Ok(MessageStatus::Handled(id))
                        };
                    }
                }

                let mut insert = connection.prepare(
                    "INSERT INTO chat_messages
                         (repository, server, channel, irc_message, account, author)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     RETURNING id",
                )?;
                insert.bind((1, repository.as_str()))?;
                insert.bind((2, server.as_str()))?;
                insert.bind((3, channel.as_str()))?;
                insert.bind((4, network_message.as_deref()))?;
                insert.bind((5, account.as_str()))?;
                insert.bind((6, author.as_str()))?;
                insert.next()?;
                message_id(insert.read::<i64, _>("id")?).map(MessageStatus::Pending)
            })
            .await
            .map_err(Error::from)
    }

    /// Marks a message handled after agent delivery and publication recovery succeed.
    pub(crate) async fn mark_handled(&self, message: MessageId) -> Result<(), Error> {
        let updated = self
            .database
            .call(move |connection| {
                let mut statement = connection
                    .prepare("UPDATE chat_messages SET handled = 1 WHERE id = ?1 RETURNING id")?;
                statement.bind((1, message.get()))?;
                Ok(statement.next()? == SqliteState::Row)
            })
            .await?;
        if updated {
            Ok(())
        } else {
            Err(Error::MessageNotFound(message))
        }
    }

    /// Queues a reply for the channel and durable inbound message in the active chat context.
    pub(crate) async fn enqueue_reply(
        &self,
        repository: RepoId,
        channel: &ChannelId,
        in_reply_to: MessageId,
        body: String,
    ) -> Result<OutboxId, Error> {
        if body.trim().is_empty() {
            return Err(Error::EmptyReply);
        }
        let repository = repository.to_string();
        let channel = channel.to_string();
        let reply = self
            .database
            .call(move |connection| {
                let mut insert = connection.prepare(
                    "INSERT INTO chat_outbox (repository, channel, in_reply_to, body)
                     SELECT ?1, ?2, ?3, ?4
                     WHERE EXISTS (
                         SELECT 1 FROM chat_messages
                         WHERE id = ?3 AND repository = ?1 AND channel = ?2
                     )
                     RETURNING id",
                )?;
                insert.bind((1, repository.as_str()))?;
                insert.bind((2, channel.as_str()))?;
                insert.bind((3, in_reply_to.get()))?;
                insert.bind((4, body.as_str()))?;
                if insert.next()? == SqliteState::Row {
                    outbox_id(insert.read::<i64, _>("id")?).map(Some)
                } else {
                    Ok(None)
                }
            })
            .await?;
        reply.ok_or(Error::MessageContextMismatch(in_reply_to))
    }

    /// Returns unsent replies in creation order for one repository.
    pub(crate) async fn pending_replies(
        &self,
        repository: RepoId,
    ) -> Result<Vec<OutboxMessage>, Error> {
        let repository = repository.to_string();
        self.database
            .call(move |connection| {
                let mut select = connection.prepare(
                    "SELECT id, channel, in_reply_to, body FROM chat_outbox
                     WHERE repository = ?1 AND sent = 0 ORDER BY id ASC",
                )?;
                select.bind((1, repository.as_str()))?;
                let mut replies = Vec::new();
                while select.next()? == SqliteState::Row {
                    let channel = select.read::<String, _>("channel")?;
                    replies.push(OutboxMessage {
                        id: outbox_id(select.read::<i64, _>("id")?)?,
                        channel: ChannelId::from_str(&channel).map_err(|error| sqlite::Error {
                            code: None,
                            message: Some(error.to_string()),
                        })?,
                        in_reply_to: message_id(select.read::<i64, _>("in_reply_to")?)?,
                        body: select.read::<String, _>("body")?,
                    });
                }
                Ok(replies)
            })
            .await
            .map_err(Error::from)
    }

    /// Marks a queued reply sent after the IRC transport accepts it.
    pub(crate) async fn mark_reply_sent(&self, reply: OutboxId) -> Result<(), Error> {
        let updated = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "UPDATE chat_outbox SET sent = 1, sent_at = unixepoch()
                     WHERE id = ?1 RETURNING id",
                )?;
                statement.bind((1, reply.get()))?;
                Ok(statement.next()? == SqliteState::Row)
            })
            .await?;
        if updated {
            Ok(())
        } else {
            Err(Error::OutboxMessageNotFound(reply))
        }
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ChatState").finish_non_exhaustive()
    }
}

fn message_id(value: i64) -> sqlite::Result<MessageId> {
    MessageId::new(value).map_err(|error| sqlite::Error {
        code: None,
        message: Some(error.to_string()),
    })
}

fn outbox_id(value: i64) -> sqlite::Result<OutboxId> {
    if value > 0 {
        Ok(OutboxId(value))
    } else {
        Err(sqlite::Error {
            code: None,
            message: Some(format!("invalid chat outbox database identifier {value}")),
        })
    }
}

/// Failure preparing or updating durable chat state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Preparing the state directory failed.
    #[error("chat state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Opening or updating the database failed.
    #[error("chat database error: {0}")]
    Database(#[from] crate::database::Error),
    /// A supplied IRC message ID was empty and cannot support deduplication.
    #[error("IRC message ID must not be empty")]
    InvalidNetworkMessageId,
    /// A message absent from durable state cannot be acknowledged.
    #[error("chat message {0} does not exist")]
    MessageNotFound(MessageId),
    /// Empty messages cannot be sent to IRC.
    #[error("chat reply must not be empty")]
    EmptyReply,
    /// The reply did not match a durable inbound message and its channel.
    #[error("chat message {0} does not belong to the active repository and channel")]
    MessageContextMismatch(MessageId),
    /// An outbox message absent from durable state cannot be acknowledged.
    #[error("chat outbox message {} does not exist", .0.get())]
    OutboxMessageNotFound(OutboxId),
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{fs, path::Path, str::FromStr as _};

    use radicle::{crypto::PublicKey, identity::RepoId};

    use crate::chat::{
        ChannelId,
        state::{MessageStatus, Receipt, State},
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    fn directory() -> tempfile::TempDir {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tests/chat");
        fs::create_dir_all(&target).expect("chat test directory should be created");
        tempfile::tempdir_in(target).expect("temporary chat directory should be created")
    }

    #[tokio::test]
    async fn deduplicates_history_and_retries_only_unhandled_messages() {
        let root = directory();
        let state = State::open(&root.path().join("state.db"))
            .await
            .expect("chat state should open");
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let channel = ChannelId::from_str("general").expect("channel should be valid");
        let author = radicle::identity::Did::from(
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
        );

        let record = |receipt| state.record(repository, "team", &channel, receipt, "alice", author);
        let first = record(Receipt::History { message: "irc-1" })
            .await
            .expect("history should be recorded");
        assert!(matches!(first, MessageStatus::Pending(_)));
        assert_eq!(
            record(Receipt::History { message: "irc-1" })
                .await
                .expect("interrupted history should be retried"),
            first
        );

        state
            .mark_handled(first.id())
            .await
            .expect("message should be marked handled");
        assert_eq!(
            record(Receipt::History { message: "irc-1" })
                .await
                .expect("handled history should be recognized"),
            MessageStatus::Handled(first.id())
        );
    }

    #[tokio::test]
    async fn permits_unidentified_live_messages_but_not_unidentified_history() {
        let root = directory();
        let state = State::open(&root.path().join("state.db"))
            .await
            .expect("chat state should open");
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let channel = ChannelId::from_str("general").expect("channel should be valid");
        let author = radicle::identity::Did::from(
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
        );

        let first = state
            .record(
                repository,
                "team",
                &channel,
                Receipt::Live { message: None },
                "alice",
                author,
            )
            .await
            .expect("live message should receive a local ID");
        let second = state
            .record(
                repository,
                "team",
                &channel,
                Receipt::Live { message: None },
                "alice",
                author,
            )
            .await
            .expect("another live message should receive its own local ID");
        assert_ne!(first.id(), second.id());

        state
            .record(
                repository,
                "team",
                &channel,
                Receipt::History { message: "" },
                "alice",
                author,
            )
            .await
            .expect_err("history without a stable ID must not enter delivery");
    }

    #[tokio::test]
    async fn persists_ordered_replies_for_the_active_message_until_sent() {
        let root = directory();
        let path = root.path().join("state.db");
        let state = State::open(&path).await.expect("chat state should open");
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let channel = ChannelId::from_str("general").expect("channel should be valid");
        let author = radicle::identity::Did::from(
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
        );
        let inbound = state
            .record(
                repository,
                "team",
                &channel,
                Receipt::Live {
                    message: Some("irc-1"),
                },
                "alice",
                author,
            )
            .await
            .expect("inbound message should be recorded")
            .id();
        state
            .enqueue_reply(
                repository,
                &ChannelId::from_str("other").expect("channel should be valid"),
                inbound,
                "Wrong channel".to_owned(),
            )
            .await
            .expect_err("a reply must match its durable inbound channel");
        let first = state
            .enqueue_reply(repository, &channel, inbound, "First".to_owned())
            .await
            .expect("first reply should be queued");
        let second = state
            .enqueue_reply(repository, &channel, inbound, "Second".to_owned())
            .await
            .expect("second reply should be queued");
        drop(state);

        let state = State::open(&path).await.expect("chat state should reopen");
        let pending = state
            .pending_replies(repository)
            .await
            .expect("outbox should be readable");
        assert_eq!(
            pending
                .iter()
                .map(|reply| {
                    (
                        reply.id,
                        reply.channel.as_str(),
                        reply.in_reply_to,
                        reply.body.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (first, "general", inbound, "First"),
                (second, "general", inbound, "Second"),
            ]
        );

        state
            .mark_reply_sent(first)
            .await
            .expect("sent reply should be acknowledged");
        assert_eq!(
            state
                .pending_replies(repository)
                .await
                .expect("outbox should remain readable")
                .into_iter()
                .map(|reply| reply.id)
                .collect::<Vec<_>>(),
            [second]
        );
    }
}
