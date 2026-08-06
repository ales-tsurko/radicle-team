//! Agent lifecycle and typed delivery interface for classified Radicle activity.
//!
//! An adapter starts with the selected [`Profile`](crate::member::Profile), a run-scoped
//! [`McpEndpoint`](crate::agent::McpEndpoint), and a preconfigured
//! [`AgentSandbox`](crate::agent::AgentSandbox). It receives typed interaction methods and stops
//! before that endpoint is removed. Repository context and actions are exposed by the
//! delivery-scoped MCP server.

pub use crate::{
    agent::interface::{
        Adapter, ChatContext, ChatMessage, Context, DeliveryId, Error, InvalidSource, Layout,
        McpEndpoint, Source, WorkId,
    },
    sandbox::{AgentChild, AgentCommand, AgentSandbox},
    watcher::{Change, Comment, Subject, Target},
};
pub(crate) use crate::agent::{
    interface::{Chatter, Coder, Idle, Stall, Supervised, Supervisor, Talker},
    publisher::{Authorizer, Error as PublishError, MutationKey, MutationValidation, Publisher},
};
#[cfg(all(test, not(miri)))]
pub(crate) use crate::agent::publisher::Announcer;

mod interface;
mod publisher;
