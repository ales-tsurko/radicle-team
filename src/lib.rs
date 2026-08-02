//! Identity, policy, node lifecycle, and activity watching for `radicle-team`.
//!
//! # Architecture
//!
//! One [`Application`] represents one fixed [`member::Member`] working in one Radicle repository.
//! The application owns the member's isolated home, signing identity, node, repository policy, and
//! watcher state. `setup` provisions and validates those resources; `run` keeps them alive while
//! repository activity is observed.
//!
//! # Two halves of one member
//!
//! A member answers messages while it builds, and one conversation cannot do both: a half-written
//! patch has no safe point to pause at, so a question would wait behind a twenty-minute job. Each
//! member therefore runs two adapters under one identity, one MCP server each. To everybody
//! reading the repository it is one person.
//!
//! [`agent::Talker`] answers. [`agent::Coder`] builds. Being assigned is the one thing that is not
//! a message, so it becomes a job in the member's [`queue`] instead of reaching the conversation;
//! the talking half can add jobs of its own, send a correction to the running one, or ask it to
//! stop. [`builder::Builder`] takes jobs and drives the building half alongside the watcher.
//!
//! The runtime pipeline is:
//!
//! ```text
//! Radicle node event
//!     -> issue and patch history scan
//!     -> Radicle action normalization
//!     -> member-specific classification
//!     -> internal activity
//!     -> typed Talker method            (a message)
//!        or the member's work queue     (an assignment)
//!            -> Coder::work
//!     -> work-scoped MCP reads and writes
//! ```
//!
//! [`watcher::Watcher`] rebuilds issue and patch state from their operation histories whenever a
//! relevant reference is fetched. Issue- and patch-specific adapters translate Radicle actions into
//! one normalized model. The classifier then determines whether an operation represents an
//! assignment, mention, reply, or relevant change for the selected member and produces an internal
//! activity. This keeps Radicle's low-level action variants out of consumers.
//!
//! A dispatcher maps each deliverable activity to the typed `mentioned`, `replied`, or `changed`
//! method. Runtime-specific adapters implement these interfaces; the core does not depend on an
//! agent vendor API. Each callback gets a fresh working copy and returns only success or failure.
//! Repository actions are separate MCP calls, so an agent can perform any number of allowed
//! actions while one unit of work is active.
//!
//! During `run`, the application starts two loopback-only MCP servers scoped to the selected
//! repository and member profile. `start` receives the provider-neutral member profile, its
//! unguessable capability URL, and a preconfigured [`sandbox::AgentSandbox`] before any work is
//! delivered. Manifest allowlists control the exposed read, comment, issue, patch, and review
//! methods; the split between the halves only withholds what each could not use anyway. Tools are
//! usable only while one delivery or job is active and cannot address a different subject. Both
//! adapters are stopped before the servers and node shut down.
//!
//! # State and delivery
//!
//! Full history replay is required because later operations depend on earlier assignment and
//! ownership state. Successful action deliveries are stored in SQLite so rescans do not report
//! them twice; each delivery is recorded after its adapter call succeeds. Adapters receive a stable
//! [`agent::DeliveryId`] in [`agent::Context`]. Its action position distinguishes multiple
//! deliveries from one Radicle operation. Repository writes are journalled under an
//! [`agent::WorkId`], which names either a delivery or a job, so an interrupted write is recovered
//! by whichever half issued it.
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
/// Queued work carried out by a member's building half.
pub mod builder;
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
/// Durable per-member work queue.
pub mod queue;
/// Local process sandboxing for agent adapters.
pub mod sandbox;
mod shutdown;
/// Radicle activity watching and agent delivery.
pub mod watcher;
mod workspace;
