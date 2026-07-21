//! Identity, policy, node lifecycle, and activity watching for `radicle-team`.
//!
//! # Architecture
//!
//! One [`Application`] represents one fixed [`member::Member`] working in one Radicle repository.
//! The application owns the member's isolated home, signing identity, node, repository policy, and
//! watcher state. `setup` provisions and validates those resources; `run` keeps them alive while
//! repository activity is observed.
//!
//! The runtime pipeline is:
//!
//! ```text
//! Radicle node event
//!     -> issue and patch history scan
//!     -> Radicle action normalization
//!     -> member-specific classification
//!     -> Activity
//!     -> delivery
//! ```
//!
//! [`watcher::Watcher`] rebuilds issue and patch state from their operation histories whenever a
//! relevant reference is fetched. Issue- and patch-specific adapters translate Radicle actions into
//! one normalized model. The classifier then determines whether an operation represents an
//! assignment, mention, reply, or relevant change for the selected member and produces a
//! [`watcher::Activity`]. This keeps Radicle's low-level action variants out of consumers.
//!
//! Delivery currently reports classified activities through a callback; the command-line
//! application logs them. The agent integration boundary will replace that final behavior with one
//! `Agent` interface. A dispatcher will map each activity kind to a typed method such as
//! `assigned`, `mentioned`, `replied`, or `changed`. Agent adapters for Codex, Claude Code, and
//! other runtimes will implement that interface and will not receive the internal activity model
//! directly.
//!
//! # State and delivery
//!
//! Full history replay is required because later operations depend on earlier assignment and
//! ownership state. Processed operation IDs are stored in SQLite so rescans do not report the same
//! operation twice. Once agent execution is introduced, successful delivery must be recorded only
//! after the adapter completes; failures must remain retryable without duplicating completed work.
//!
//! # Trust boundaries
//!
//! A member runs under a dedicated Radicle identity and node. [`manifest`] binds the fixed member
//! name to the permitted signing DID and a monotonic generation, preventing a stale or replaced
//! local identity from continuing silently. Private keys and watcher state remain in the isolated
//! member home, while the repository manifest contains only shared identity policy. The application
//! validates this binding after synchronization and before watching activity.

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
