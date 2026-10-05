//! MCP interface scoped to each agent activity for Radicle collaboration.
//!
//! Transport and activity scope live in `server`, request types and conversion in `protocol`,
//! routing in `tools`, and repository operations in `read`, `write`, and `recovery`.

pub(crate) use crate::mcp::{
    server::{ActivityContext, ActivityScope, Error, Server},
    tools::{ToolService, ToolState},
};

mod artifacts;
mod context;
mod handoff;
mod protocol;
mod read;
mod recovery;
mod review;
mod security;
mod server;
mod tasks;
mod tools;
mod write;

#[cfg(test)]
mod tests;
