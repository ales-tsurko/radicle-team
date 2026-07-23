//! Agent lifecycle and typed delivery interface for classified Radicle activity.
//!
//! An adapter starts with a run-scoped [`McpEndpoint`](crate::agent::McpEndpoint), receives typed
//! interaction methods, and stops before that endpoint is removed. Repository context and actions
//! are exposed by the delivery-scoped MCP server.

pub use crate::{
    agent::interface::{Agent, Context, DeliveryId, Error, McpEndpoint},
    watcher::{Change, Comment, Subject, Target},
};
pub(crate) use crate::agent::{
    interface::{LegacyPendingReply, LegacyReply, LegacyReplyTarget},
    publisher::{Authorizer, MutationKey, MutationValidation, Publisher},
};
#[cfg(all(test, not(miri)))]
pub(crate) use crate::agent::publisher::{Announcer, Error as PublishError};

mod interface;
mod publisher;
