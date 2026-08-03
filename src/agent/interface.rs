//! Agent callback interface and delivery value types.

use std::{
    error::Error as StdError,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use async_trait::async_trait;
use radicle::{
    cob::EntryId,
    identity::{Did, RepoId},
};
use url::Url;

use crate::{
    chat::{AuthenticatedSender, ChannelId, MessageId as ChatMessageId},
    member::Profile,
    queue::Job,
    sandbox::AgentSandbox,
    watcher::{Change, Comment, Subject, Target},
};

/// A backend capable of answering activity addressed to one team member.
///
/// Being assigned is the one thing this never sees: it is work rather than a message, so it joins
/// the member's queue and reaches [`Coder`] instead.
#[async_trait]
pub trait Talker: Send {
    /// Configures and starts the adapter for one member and run-scoped MCP server.
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
    ) -> Result<(), Error>;

    /// Handles a mention in an issue or patch.
    async fn mentioned(&mut self, context: Context, target: Target) -> Result<(), Error>;

    /// Handles an authenticated message which addresses this member in chat.
    async fn messaged(&mut self, context: ChatContext, message: ChatMessage) -> Result<(), Error>;

    /// Handles a reply to one of the member's comments.
    async fn replied(
        &mut self,
        context: Context,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<(), Error>;

    /// Handles another relevant change to an owned or assigned issue or patch.
    async fn changed(&mut self, context: Context, change: Change) -> Result<(), Error>;

    /// Stops the adapter and releases its resources.
    async fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// A backend which carries out queued work for one team member.
///
/// A member answers messages and builds at the same time, which one conversation cannot do: a
/// half-written patch has no safe point to pause at. The two run as separate adapters under one
/// identity, so this interface receives jobs rather than activity. Deciding what should be built
/// belongs to [`Talker`]. Deciding how belongs here.
#[async_trait]
pub trait Coder: Send + Sync {
    /// Configures and starts the adapter for one member and run-scoped MCP server.
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
    ) -> Result<(), Error>;

    /// Carries out one queued job in `workspace`.
    ///
    /// The job's brief states what was decided and its source addresses the discussion behind it.
    /// A job may arrive from a conversation this adapter never saw.
    ///
    /// This takes a shared reference so that [`interrupt`](Self::interrupt) can reach the adapter
    /// while the job is still running.
    async fn work(&self, job: Job, workspace: PathBuf) -> Result<(), Error>;

    /// Asks the adapter to stop the running job and preserve what it has.
    ///
    /// Work in progress is committed to a branch rather than discarded, because the request to
    /// stop usually means priorities moved, not that the work was worthless. The interrupted
    /// [`work`](Self::work) call returns once that has happened.
    async fn interrupt(&self) -> Result<(), Error>;

    /// Stops the adapter and releases its resources.
    async fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Context supplied for one classified repository activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// Repository containing the interaction.
    pub repository: RepoId,
    /// Issue or patch receiving the interaction.
    pub subject: Subject,
    /// Stable identity of this individual delivery within the operation.
    pub delivery: DeliveryId,
    /// Author of the operation.
    pub author: Did,
    /// Fresh checkout prepared for this delivery.
    pub workspace: PathBuf,
}

/// Repository context supplied for one authenticated chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatContext {
    /// Repository whose bound channel received the message.
    repository: RepoId,
    /// Logical manifest binding for the IRC channel.
    channel: ChannelId,
    /// Durable local message key.
    message: ChatMessageId,
    /// Radicle identity attested for the sender's authenticated IRC account.
    author: Did,
    /// Fresh checkout prepared for this message.
    workspace: PathBuf,
}

impl ChatContext {
    /// Constructs context from sender evidence which only chat authorization can produce.
    #[cfg_attr(
        any(not(test), miri),
        expect(
            dead_code,
            reason = "the authenticated IRC ingress will construct this context in PR 6"
        )
    )]
    pub(crate) fn authenticated(
        repository: RepoId,
        channel: ChannelId,
        message: ChatMessageId,
        sender: AuthenticatedSender,
        workspace: PathBuf,
    ) -> Self {
        Self {
            repository,
            channel,
            message,
            author: sender.did(),
            workspace,
        }
    }

    /// Returns the repository bound to the authenticated channel.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.repository
    }

    /// Returns the logical manifest channel binding.
    #[must_use]
    pub const fn channel(&self) -> &ChannelId {
        &self.channel
    }

    /// Returns the durable message key.
    #[must_use]
    pub const fn message(&self) -> ChatMessageId {
        self.message
    }

    /// Returns the DID attested for the authenticated IRC account.
    #[must_use]
    pub const fn author(&self) -> Did {
        self.author
    }

    /// Returns the fresh checkout prepared for this message.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

/// Authenticated chat message delivered to the talking adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// Sender's transient IRC nickname, for presentation only.
    pub nick: String,
    /// Message text.
    pub body: String,
}

/// Place from which a unit of work originated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Radicle issue or patch activity.
    Subject(Subject),
    /// Authenticated chat in a repository-bound channel.
    Chat(ChannelId),
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Subject(subject) => subject.fmt(formatter),
            Self::Chat(channel) => write!(formatter, "chat:{channel}"),
        }
    }
}

impl FromStr for Source {
    type Err = InvalidSource;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(channel) = value.strip_prefix("chat:") {
            return ChannelId::from_str(channel)
                .map(Self::Chat)
                .map_err(|_error| InvalidSource(value.to_owned()));
        }
        Subject::from_str(value)
            .map(Self::Subject)
            .map_err(|_error| InvalidSource(value.to_owned()))
    }
}

impl From<Subject> for Source {
    fn from(subject: Subject) -> Self {
        Self::Subject(subject)
    }
}

/// A stored work source could not be read back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid work source '{0}'")]
pub struct InvalidSource(String);

/// Idempotency key for one classified action delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryId {
    /// Radicle operation containing the action.
    pub operation: EntryId,
    /// Zero-based action position within the operation.
    pub action: usize,
}

impl fmt::Display for DeliveryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.operation, self.action)
    }
}

/// One unit of work a member's repository writes belong to.
///
/// The talker handles one classified delivery or chat message at a time and the coder one queued
/// job, so the two run under separate scopes even though they publish under a single identity.
/// Recovering an interrupted write means asking which scope it belonged to, which is why chat and
/// jobs need identities the delivery key cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkId {
    /// One classified action delivered to the talker.
    Delivery(DeliveryId),
    /// One authenticated chat message.
    Chat(ChatMessageId),
    /// One queued job carried out by the coder.
    Job(i64),
}

impl WorkId {
    /// Returns a name for this work usable as a directory on every supported platform.
    #[must_use]
    pub fn directory(&self) -> String {
        self.to_string().replace(':', "-")
    }
}

impl fmt::Display for WorkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delivery(delivery) => write!(formatter, "{delivery}"),
            Self::Chat(message) => write!(formatter, "chat:{message}"),
            Self::Job(job) => write!(formatter, "job:{job}"),
        }
    }
}

impl From<DeliveryId> for WorkId {
    fn from(delivery: DeliveryId) -> Self {
        Self::Delivery(delivery)
    }
}

impl From<ChatMessageId> for WorkId {
    fn from(message: ChatMessageId) -> Self {
        Self::Chat(message)
    }
}

/// Address of the scoped MCP server available to an agent while it runs.
///
/// This remains a newtype instead of an alias for [`Url`] because the URL embeds a capability
/// token. The wrapper prevents confusing it with arbitrary URLs and provides a redacted
/// [`Debug`](std::fmt::Debug) implementation so the capability is not logged accidentally.
#[derive(Clone, PartialEq, Eq)]
pub struct McpEndpoint(Url);

impl McpEndpoint {
    /// Wraps a validated Streamable HTTP MCP URL.
    #[must_use]
    pub const fn new(url: Url) -> Self {
        Self(url)
    }

    /// Returns the validated Streamable HTTP MCP URL.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.0
    }
}

impl std::fmt::Debug for McpEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpEndpoint")
            .field("transport", &"loopback HTTP")
            .finish_non_exhaustive()
    }
}

/// Error returned by an agent adapter.
#[derive(Debug, thiserror::Error)]
#[error("agent adapter failed: {source}")]
pub struct Error {
    source: Box<dyn StdError + Send + Sync>,
}

impl Error {
    /// Wraps an adapter-specific error.
    pub fn new(source: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use radicle::cob::EntryId;
    use url::Url;

    use crate::{
        agent::{DeliveryId, McpEndpoint, Source, WorkId},
        chat::MessageId,
        watcher::Subject,
    };

    #[test]
    fn work_directory_names_avoid_characters_a_path_cannot_hold() {
        let delivery = WorkId::Delivery(DeliveryId {
            operation: EntryId::from_str("1111111111111111111111111111111111111111")
                .expect("operation should be valid"),
            action: 2,
        });

        for work in [
            delivery,
            WorkId::Chat(MessageId::new(3).expect("message should be valid")),
            WorkId::Job(7),
        ] {
            let directory = work.directory();
            assert!(
                !directory.contains(':'),
                "Windows rejects a colon in a path, so '{directory}' would fail there"
            );
            assert!(directory.contains(&work.to_string().replace(':', "-")));
        }
    }

    #[test]
    fn work_sources_round_trip_through_storage() {
        let subject = Subject::Issue(
            radicle::cob::ObjectId::from_str("1111111111111111111111111111111111111111")
                .expect("subject should be valid"),
        );
        for source in [
            Source::Subject(subject),
            Source::Chat("general".parse().expect("channel should be valid")),
        ] {
            assert_eq!(source.to_string().parse::<Source>(), Ok(source));
        }
    }

    #[test]
    fn endpoint_debug_output_hides_its_capability() {
        let endpoint = McpEndpoint::new(
            Url::parse("http://127.0.0.1:1234/mcp/secret").expect("endpoint should be valid"),
        );
        let debug = format!("{endpoint:?}");

        assert!(!debug.contains("secret"));
        assert_eq!(endpoint.url().as_str(), "http://127.0.0.1:1234/mcp/secret");
    }
}
