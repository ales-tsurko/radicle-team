//! Reaching Anthropic's models through Claude Code.
//!
//! One adapter, for the agent application a provider file names `claude-code`. Like its OpenCode
//! counterpart it holds the conversation with the model, carries out the tool calls, and edits the
//! working copy it is started in, while deciding nothing: the model comes from the provider file,
//! the documents come from the member, the tools come from the run-scoped MCP server, and what may
//! be touched comes from the sandbox.
//!
//! One delivery is one `claude -p`. Nothing is resumed between deliveries -- sessions are not even
//! written to disk -- because an agent reads the issue, patch, or chat message it is answering
//! through MCP, which is the record everybody else reads too. Claude Code streams each event as it
//! happens on that run's output, so unlike OpenCode it needs no plugin for the runtime to see it
//! working.

pub(crate) use crate::agent::claude_code::harness::{ADAPTER, ClaudeCode, Error};

mod config;
mod harness;
pub(crate) mod run;

#[cfg(test)]
mod tests;
