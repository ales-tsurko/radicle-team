//! Delivery-scoped MCP interface for Radicle collaboration.
//!
//! Transport and delivery scope live in `server`, request types and conversion in `protocol`,
//! routing in `tools`, and repository operations in `read`, `write`, and `recovery`.

pub(crate) use crate::mcp::{
    server::{DeliveryScope, Error, Server},
    tools::ToolService,
};

mod protocol;
mod read;
mod recovery;
mod server;
mod tools;
mod write;

#[cfg(test)]
mod tests;
