//! Agent callback interface and delivery value types.

use std::{
    error::Error as StdError,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
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

/// How long an agent may show no progress before the runtime treats it as stuck.
///
/// Nothing bounds a delivery's duration, because a real one can run for hours.
pub(crate) const SILENCE: Duration = Duration::from_secs(15 * 60);
/// How often the runtime asks an adapter whether its agent is still doing anything.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(10);

/// A role is the kind of work one of a member's agents takes — three run concurrently under its
/// single Radicle identity, so one kind of work does not wait behind another.
///
/// Each has its own MCP server, its own working copy, and its own harness state, so none can
/// disturb another's checked-out branch or read another's transcripts. The build cache is the one
/// thing they share, because build output is too large to keep three copies of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Answers repository activity.
    Talker,
    /// Answers authenticated chat.
    Chatter,
    /// Carries out queued work.
    Coder,
}

impl Role {
    /// Every role, in a stable order.
    pub const ALL: [Self; 3] = [Self::Talker, Self::Chatter, Self::Coder];

    /// Returns the fixed directory name for this role.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Talker => "talker",
            Self::Chatter => "chatter",
            Self::Coder => "coder",
        }
    }

    /// Returns whether this role's working copy stands on a branch.
    #[must_use]
    pub const fn owns_branches(self) -> bool {
        matches!(self, Self::Coder)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// What one role is given for the life of the member.
///
/// Delivery workspaces arrive per call. These do not: a build cache survives between deliveries on
/// purpose, a harness needs somewhere of its own to keep settings and logs, and the documents
/// instructing the role are the same ones from its first delivery to its last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    role: Role,
    cache: PathBuf,
    harness: PathBuf,
    documents: Vec<PathBuf>,
}

impl Layout {
    /// Describes what belongs to `role`, instructed by the documents at `documents`.
    #[must_use]
    pub const fn new(
        role: Role,
        cache: PathBuf,
        harness: PathBuf,
        documents: Vec<PathBuf>,
    ) -> Self {
        Self {
            role,
            cache,
            harness,
            documents,
        }
    }

    /// Returns which role these directories belong to.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Returns the writable cache directory for build and dependency output.
    ///
    /// Point `CARGO_TARGET_DIR` and its equivalents here, so build output stays out of the working
    /// copy. Shared with this member's other roles, which may make them wait on a build lock.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// Returns the directory the agent harness keeps its own state in.
    ///
    /// A harness writes settings, logs and transcripts under a user-level directory of its own by
    /// default, which every member on the machine would then share. Point it here instead, so one
    /// member's harness state stays that member's and goes when its directory goes.
    #[must_use]
    pub fn harness(&self) -> &Path {
        &self.harness
    }

    /// Returns the documents instructing this role, in the order they should be read.
    ///
    /// The documents are compiled into the binary, which ships without the repository they were
    /// written in. The runtime writes this role's set out with
    /// [`documents::install`](crate::agent::documents::install) before starting it and passes the
    /// files here, because a harness reads its instructions from disk. They stay read-only to the
    /// agent, so nothing can reword its own character mid-run.
    ///
    /// An adapter whose harness takes instructions some other way reads the files instead; the
    /// order is the one the documents themselves establish.
    #[must_use]
    pub fn documents(&self) -> &[PathBuf] {
        &self.documents
    }
}

/// Hands the runtime a shared view of a running agent.
///
/// The handle is separate from the adapter because the runtime watches an agent while that same
/// adapter is busy handling the delivery, so the two cannot both borrow it.
pub(crate) trait Supervised: Send {
    /// Returns a handle which can watch and stop this adapter's agent.
    fn supervisor(&self) -> Arc<dyn Supervisor>;
}

/// The part of every adapter the runtime supervises.
///
/// An agent can run for hours and still be working, so nothing bounds how long one may take. What
/// is bounded is silence. Each adapter reports when it last saw its own agent do something, and
/// the runtime decides what counts as too long and stops it. Reading that signal out of a
/// particular harness belongs to the adapter, deciding what to do about it does not.
#[async_trait]
pub(crate) trait Supervisor: Send + Sync {
    /// Reports when this adapter last saw its agent do anything.
    fn last_progress(&self) -> Option<Instant>;

    /// Force-stops whatever the agent is doing, whether it agreed to or not.
    ///
    /// Weaker than [`Talker::shutdown`] and its counterparts: this ends one delivery, and the
    /// agent stays running for the next one.
    async fn abort(&self) -> Result<(), Error>;
}

/// Watches one agent for silence, so the two places which run agents agree on what stuck means.
pub(crate) struct Stall {
    watch: Arc<dyn Supervisor>,
    seen: Instant,
}

impl fmt::Debug for Stall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Stall").finish_non_exhaustive()
    }
}

impl Stall {
    pub(crate) fn new(watch: Arc<dyn Supervisor>) -> Self {
        Self {
            watch,
            seen: Instant::now(),
        }
    }

    /// Waits one heartbeat and reports whether the agent has gone silent for too long.
    pub(crate) async fn silent(&mut self) -> bool {
        tokio::time::sleep(HEARTBEAT).await;
        if let Some(progress) = self.watch.last_progress() {
            self.seen = self.seen.max(progress);
        }

        self.seen.elapsed() >= SILENCE
    }

    /// Force-stops the agent, reporting how long it had been silent.
    pub(crate) async fn abort(&self) -> Result<(), Error> {
        tracing::warn!(silent_for = ?self.seen.elapsed(), "agent stopped making progress");

        self.watch.abort().await
    }
}

/// A supervisor for a role which runs no separate process.
///
/// Reports no progress and has nothing to stop, so the runtime never interferes with it. Every
/// role the runtime starts runs one, so this is what the tests which stand in for one use. Those
/// tests all spawn or wait, which Miri cannot do, so under Miri nothing here is built at all.
#[cfg(all(test, not(miri)))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Idle;

#[cfg(all(test, not(miri)))]
#[async_trait]
impl Supervisor for Idle {
    fn last_progress(&self) -> Option<Instant> {
        None
    }

    async fn abort(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// A backend capable of answering repository activity addressed to one team member.
///
/// Two things this never sees. Being assigned is work rather than a message, so it joins the
/// member's queue and reaches [`Coder`]. A chat message reaches [`Chatter`], because somebody is
/// waiting on the other end of it and cannot wait behind a twenty-minute review.
#[async_trait]
pub(crate) trait Talker: Supervised {
    /// Configures and starts the role for one member and run-scoped MCP server.
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), Error>;

    /// Handles a mention in an issue or patch.
    async fn mentioned(&mut self, context: Context, target: Target) -> Result<(), Error>;

    /// Handles a reply to one of the member's comments.
    async fn replied(
        &mut self,
        context: Context,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<(), Error>;

    /// Handles another relevant change to an owned or assigned issue or patch.
    async fn changed(&mut self, context: Context, change: Change) -> Result<(), Error>;

    /// Ends the role and releases everything it holds.
    ///
    /// Stronger than [`Supervisor::abort`], which ends one delivery and leaves the agent running.
    async fn shutdown(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// A backend which answers authenticated chat addressed to one team member.
///
/// Chat runs as its own role rather than as another kind of message for [`Talker`] because the two
/// have different latency to meet. A person typing in a channel expects an answer now, and an agent
/// turn cannot be interrupted, so sharing one conversation would put every question behind whatever
/// review happened to start first.
///
/// Deciding that chat should produce work belongs here. Carrying it out belongs to [`Coder`],
/// reached through the member's queue.
#[async_trait]
pub(crate) trait Chatter: Supervised {
    /// Configures and starts the role for one member and run-scoped MCP server.
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), Error>;

    /// Handles an authenticated message which addresses this member in a bound channel.
    async fn messaged(&mut self, context: ChatContext, message: ChatMessage) -> Result<(), Error>;

    /// Ends the role and releases everything it holds.
    ///
    /// Stronger than [`Supervisor::abort`], which ends one delivery and leaves the agent running.
    async fn shutdown(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// A backend which carries out queued work for one team member.
///
/// A member answers messages and builds at the same time, which one conversation cannot do: a
/// half-written patch has no safe point to pause at. The two run as separate roles under one
/// identity, so this interface receives jobs rather than activity. Deciding what should be built
/// belongs to [`Talker`]. Deciding how belongs here.
#[async_trait]
pub(crate) trait Coder: Supervised + Sync {
    /// Configures and starts the role for one member and run-scoped MCP server.
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), Error>;

    /// Carries out one queued job in `workspace`.
    ///
    /// The job's brief states what was decided and its source addresses the discussion behind it. A
    /// job may arrive from a conversation this role never saw.
    ///
    /// This takes a shared reference so that [`interrupt`](Self::interrupt) can reach the role
    /// while the job is still running.
    async fn work(&self, job: Job, workspace: PathBuf) -> Result<(), Error>;

    /// Asks the role to stop the running job and preserve what it has.
    ///
    /// Work in progress is committed to a branch rather than discarded, because the request to stop
    /// usually means priorities moved, not that the work was worthless. The interrupted
    /// [`work`](Self::work) call returns once that has happened.
    async fn interrupt(&self) -> Result<(), Error>;

    /// Ends the role and releases everything it holds.
    ///
    /// Stronger than [`Supervisor::abort`], which ends one delivery and leaves the agent running.
    async fn shutdown(&mut self) -> Result<(), Error> {
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

/// Authenticated chat message delivered to the talking role.
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
