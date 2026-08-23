//! Reaching a member's models through OpenCode.
//!
//! One adapter, for the agent application a provider file names `opencode`. It holds the
//! conversation with the model, carries out the tool calls, and edits the working copy it is
//! started in. What it does not do is decide anything: the model comes from the provider file, the
//! documents come from the member, the tools come from the run-scoped MCP server, and what may be
//! touched comes from the sandbox. This adapter is what puts those in front of it.
//!
//! One delivery is one `opencode run`. Nothing is resumed between deliveries: an agent reads the
//! issue, patch, or chat message it is answering through MCP, which is the record everybody else
//! reads too, so a lost transcript costs nothing and a stale one would cost correctness.
//!
//! Each role runs its own OpenCode with its own harness home, so the three never share a session
//! store, a log, or a configuration file. They do share a build cache, which is the one thing too
//! large to keep three copies of.

pub(crate) use crate::agent::opencode::harness::{ADAPTER, Error, OpenCode};

mod catalogue;
mod config;
mod harness;
pub(crate) mod run;

#[cfg(test)]
mod tests;
