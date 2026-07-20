//! Library for running an AI contributor on Radicle.

pub use crate::application::{Application, Error, Setup, SetupStatus};

mod application;
/// Command-line interface for selecting and running a team member.
pub mod cli;
/// Filesystem locations used by each team member.
pub mod home;
/// Radicle identity ownership and key management.
pub mod identity;
/// Repository-wide team manifest.
pub mod manifest;
/// Fixed team members and their profiles.
pub mod member;
/// Supervised Radicle node lifecycle.
pub mod node;
mod shutdown;
/// Read-only Radicle activity watching.
pub mod watcher;
