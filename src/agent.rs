//! Agent lifecycle and typed delivery interface for classified Radicle activity.
//!
//! An adapter starts with a run-scoped [`McpEndpoint`](crate::agent::McpEndpoint), receives typed
//! interaction methods, and stops before that endpoint is removed. The MCP server is the only
//! context-fetching interface; adapters do not receive a generic tools object or an invocation
//! wrapper.

use std::{error::Error as StdError, fmt};

use async_trait::async_trait;
use radicle::{
    cob::{
        EntryId,
        patch::{ReviewId, RevisionId},
    },
    identity::{Did, RepoId},
};
use url::Url;

pub use crate::watcher::{Change, Comment, Subject, Target};

mod publisher;
pub(crate) use publisher::{Announcer, Authorizer, Publisher};
#[cfg(all(test, not(miri)))]
pub(crate) use publisher::Error as PublishError;

/// Common context supplied to every agent method.
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
}

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

/// Address of the scoped MCP server available to an agent while it runs.
///
/// This remains a newtype instead of an alias for [`Url`] because the URL embeds a capability
/// token. The wrapper prevents confusing it with arbitrary URLs and provides a redacted
/// [`Debug`](std::fmt::Debug) implementation so the capability is not logged accidentally.
#[derive(Clone, PartialEq, Eq)]
pub struct McpEndpoint(Url);

impl McpEndpoint {
    pub(crate) const fn new(url: Url) -> Self {
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

/// Action requested by an agent after handling an interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No repository update is required.
    NoAction,
    /// Publish a reply to a Radicle discussion.
    Reply(Reply),
}

/// Reply requested by an agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reply {
    /// Reply body.
    pub body: String,
    /// Discussion receiving the reply.
    pub target: ReplyTarget,
}

/// Radicle discussion location receiving an agent reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReplyTarget {
    /// Issue discussion. `reply_to` defaults to the issue description.
    Issue {
        /// Existing issue comment receiving the reply.
        reply_to: Option<EntryId>,
    },
    /// Patch revision discussion.
    Revision {
        /// Revision containing the discussion.
        revision: RevisionId,
        /// Existing revision comment receiving the reply.
        reply_to: Option<EntryId>,
    },
    /// Patch review discussion.
    Review {
        /// Review containing the discussion.
        review: ReviewId,
        /// Existing review comment receiving the reply.
        reply_to: Option<EntryId>,
    },
}

/// A backend capable of handling activity addressed to one team member.
#[async_trait]
pub trait Agent: Send {
    /// Starts the adapter with access to the run-scoped MCP server.
    async fn start(&mut self, _mcp: &McpEndpoint) -> Result<(), Error> {
        Ok(())
    }

    /// Handles assignment to an issue or patch review.
    async fn assigned(&mut self, context: Context) -> Result<Outcome, Error>;

    /// Handles a mention in an issue or patch.
    async fn mentioned(&mut self, context: Context, target: Target) -> Result<Outcome, Error>;

    /// Handles a reply to one of the member's comments.
    async fn replied(
        &mut self,
        context: Context,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<Outcome, Error>;

    /// Handles another relevant change to an owned or assigned issue or patch.
    async fn changed(&mut self, context: Context, change: Change) -> Result<Outcome, Error>;

    /// Stops the adapter and releases its resources.
    async fn stop(&mut self) -> Result<(), Error> {
        Ok(())
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
    use url::Url;

    use super::McpEndpoint;

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
