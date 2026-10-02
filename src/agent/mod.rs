//! Agent lifecycle and typed delivery interface for classified Radicle activity.
//!
//! An adapter starts with the selected [`Profile`](crate::member::Profile), a run-scoped
//! [`McpEndpoint`], and a preconfigured [`AgentSandbox`]. It receives typed interaction methods and
//! stops before that endpoint is removed. Repository context and actions are exposed by the
//! delivery-scoped MCP server.

pub use crate::{
    agent::{
        adapter::{ADAPTERS, Unknown as UnknownAdapter},
        interface::{
            ActivityId, ChatContext, ChatMessage, Context, DeliveryId, Error, InvalidSource,
            Layout, McpEndpoint, Role, Source,
        },
    },
    sandbox::{AgentChild, AgentCommand, AgentSandbox},
    watcher::{Change, Comment, Subject, Target},
};
pub(crate) use crate::agent::{
    controls::Controls,
    interface::{Chatter, Coder, Stall, Supervised, Talker},
    publisher::{Authorizer, Error as PublishError, MutationKey, MutationValidation, Publisher},
    session::Session,
};
#[cfg(test)]
pub(crate) use crate::agent::publisher::Announcer;

pub(crate) mod adapter;
pub(crate) mod authentication;
pub(crate) mod claude_code;
pub(crate) mod controls;
pub mod documents;
mod interface;
mod opencode;
pub(crate) mod prompt;
mod publisher;
mod session;
