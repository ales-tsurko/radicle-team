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
//!     -> internal activity
//!     -> typed Agent method
//!     -> delivery-scoped MCP reads and writes
//! ```
//!
//! [`watcher::Watcher`] rebuilds issue and patch state from their operation histories whenever a
//! relevant reference is fetched. Issue- and patch-specific adapters translate Radicle actions into
//! one normalized model. The classifier then determines whether an operation represents an
//! assignment, mention, reply, or relevant change for the selected member and produces an internal
//! activity. This keeps Radicle's low-level action variants out of consumers.
//!
//! Delivery uses the [`agent::Agent`] interface. A dispatcher maps each internal activity to the
//! typed `assigned`, `mentioned`, `replied`, or `changed` method. Runtime-specific adapters
//! implement that interface; the core does not depend on an agent vendor API. Each callback gets a
//! fresh working copy and returns only success or failure. Repository actions are separate MCP
//! calls, so an agent can perform any number of allowed actions during one delivery.
//!
//! During `run`, the application also starts a loopback-only MCP server scoped to the
//! selected repository and member profile. [`agent::Agent::start`] receives its unguessable
//! capability URL before activity is delivered. Manifest allowlists control the exposed read,
//! comment, issue, patch, and review methods. Tools are usable only while one delivery is active
//! and cannot address a different subject. The adapter is stopped before the server and node shut
//! down.
//!
//! # State and delivery
//!
//! Full history replay is required because later operations depend on earlier assignment and
//! ownership state. Successful action deliveries are stored in SQLite so rescans do not report
//! them twice; each delivery is recorded after its adapter call succeeds. Adapters receive a stable
//! [`agent::DeliveryId`] in [`agent::Context`]. Its action position distinguishes multiple
//! deliveries from one Radicle operation.
//!
//! # Trust boundaries
//!
//! A member runs under a dedicated Radicle identity and node. [`manifest`] binds the fixed member
//! name to the permitted signing DID and a monotonic generation, preventing a stale or replaced
//! local identity from continuing silently. Private keys and watcher state remain in the isolated
//! member home, while the repository manifest contains only shared identity policy. The application
//! validates this binding after synchronization, before watching activity, and again immediately
//! before every agent-requested write.

pub use crate::application::{Application, Error, Setup, SetupStatus};

/// Agent adapter interface and its typed activity context.
pub mod agent;
mod application;
/// Command-line interface for selecting and running a team member.
pub mod cli;
mod database;
/// Filesystem locations used by each team member.
pub mod home;
/// Radicle identity ownership and key management.
pub mod identity;
/// Repository-wide team manifest.
pub mod manifest;
mod mcp;
/// Fixed team members and their profiles.
pub mod member;
/// Supervised Radicle node lifecycle.
pub mod node;
mod shutdown;
/// Radicle activity watching and agent delivery.
pub mod watcher;
mod workspace;
