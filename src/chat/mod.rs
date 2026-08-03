//! Chat identities and durable delivery state.

use std::{fmt, str::FromStr};

pub(crate) use crate::chat::{auth::AuthenticatedSender, state::State};

mod auth;
mod state;

/// Logical repository-local channel identifier from the team manifest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(String);

impl ChannelId {
    /// Returns the configured identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ChannelId {
    type Err = InvalidChannelId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(InvalidChannelId(value.to_owned()));
        }
        Ok(Self(value.to_owned()))
    }
}

/// A logical channel identifier is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid logical chat channel '{0}'")]
pub struct InvalidChannelId(String);

/// Durable local key assigned to one received chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(i64);

impl MessageId {
    /// Creates a message key from its positive database identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not positive.
    pub fn new(value: i64) -> Result<Self, InvalidMessageId> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(InvalidMessageId(value))
        }
    }

    /// Returns the database identifier.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A local chat message key is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid chat message database identifier {0}")]
pub struct InvalidMessageId(i64);

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use crate::chat::{ChannelId, MessageId};

    #[test]
    fn validates_logical_channel_and_message_identifiers() {
        for channel in ["general", "security_reports", "release-1"] {
            assert_eq!(
                ChannelId::from_str(channel)
                    .expect("channel should parse")
                    .as_str(),
                channel
            );
        }
        for channel in ["", "has space", "irc:#raw"] {
            ChannelId::from_str(channel).expect_err("channel should be rejected");
        }
        assert_eq!(MessageId::new(1).expect("message should parse").get(), 1);
        MessageId::new(0).expect_err("zero should be rejected");
    }
}
