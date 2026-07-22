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
//!     -> validated Radicle reply
//! ```
//!
//! [`watcher::Watcher`] rebuilds issue and patch state from their operation histories whenever a
//! relevant reference is fetched. Issue- and patch-specific adapters translate Radicle actions into
//! one normalized model. The classifier then determines whether an operation represents an
//! assignment, mention, reply, or relevant change for the selected member and produces an internal
//! activity. This keeps Radicle's low-level action variants out of consumers.
//!
//! Delivery uses the [`agent::Agent`] interface. A dispatcher maps each internal activity to the
//! typed `assigned`, `mentioned`, `replied`, or `changed` method. Agent adapters for Codex, Claude
//! Code, and other runtimes implement that interface and never receive the internal activity model
//! directly. [`Application::run_with_agent`] supplies an adapter; [`Application::run`] keeps the
//! command-line application's current logging behavior. An adapter can return a typed
//! [`agent::Outcome::Reply`]; the controller validates its discussion target and publishes it with
//! the managed member identity.
//!
//! During `run`, the application also starts a loopback-only, read-only MCP server scoped to the
//! selected repository and member profile. [`agent::Agent::start`] receives its unguessable
//! capability URL before activity is delivered. The server exposes `get_subject`, `get_entry`,
//! paginated `list_timeline`, `get_thread`, `get_review`, and `get_revision` tools, allowing an
//! adapter to send a small event prompt while the agent fetches only the additional Radicle context
//! it needs. The adapter is stopped before the MCP server and managed node are shut down.
//!
//! # State and delivery
//!
//! Full history replay is required because later operations depend on earlier assignment and
//! ownership state. Successful action deliveries are stored in SQLite so rescans do not report
//! them twice; each delivery is recorded immediately after its adapter call and requested publish
//! succeed, so a later failure does not replay earlier successes. Adapters receive a stable
//! [`agent::DeliveryId`] in
//! [`agent::Context`] and must use it as an idempotency key because a failure after an external
//! side effect can cause redelivery. Its action position distinguishes multiple deliveries from one
//! Radicle operation. Controller-published replies carry the same delivery ID in their commit
//! message, so a retry can detect an already-published result. New and retried replies are
//! persisted before publication, re-sign repository refs when recovered, and are announced through
//! the managed node before their deliveries are checkpointed.
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
mod context;
mod database;
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
/// Radicle activity watching and agent-outcome publication.
pub mod watcher;
