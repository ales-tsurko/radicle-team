//! Persistent identities for received IRC messages.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the IRC ingress will consume this API in PR 6")
)]

use std::path::Path;

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
}

fn message_id(value: i64) -> sqlite::Result<MessageId> {
    MessageId::new(value).map_err(|error| sqlite::Error {
        code: None,
        message: Some(error.to_string()),
    })
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
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{crypto::PublicKey, identity::RepoId};

    use crate::chat::{
        ChannelId,
        state::{MessageStatus, Receipt, State},
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[tokio::test]
    async fn deduplicates_history_and_retries_only_unhandled_messages() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
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
        let root = tempfile::tempdir().expect("temporary directory should be created");
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
}
