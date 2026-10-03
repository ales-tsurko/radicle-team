//! # Architecture
//!
//! One [`Application`] represents one fixed [`member::Member`] working in one Radicle repository.
//! The application owns the member's isolated home, signing identity, node, repository policy, and
//! watcher state. `setup` provisions and validates those resources, and `run` keeps them alive
//! while repository activity is observed.
//!
//! # Three roles, one member
//!
//! A role is the kind of work one of a member's agents takes — three run concurrently under its
//! single Radicle identity, so one kind of work does not wait behind another.
//!
//! A member answers, chats, and builds at once, and one conversation cannot do all three. A
//! half-written patch has no safe point to pause at, and an agent turn cannot be interrupted, so
//! sharing one would put a question behind a twenty-minute task. Each member therefore runs three
//! agents under one identity, one per role, with one MCP server each. To everybody reading the
//! repository it is one person.
//!
//! `Talker` answers repository activity. `Chatter` answers chat. `Coder` builds. An assignment is
//! the one thing that is not a message, so it becomes a task in the member's [`queue`] instead of
//! reaching a conversation. `Talker` and `Chatter` can both add tasks of their own, send a
//! correction to the running task, or ask the builder to stop it. [`builder::Builder`] takes tasks
//! and drives the `Coder` role alongside the other two.
//!
//! Chat is what makes the third role necessary rather than convenient, and it is the reason
//! enabling chat costs a third concurrent agent whenever a message arrives. A member with no
//! `[chat]` section in the manifest never starts one.
//!
//! The runtime pipeline is:
//!
//! ```text
//! Radicle node event
//!   - issue and patch history scan
//!   - Radicle action normalization
//!   - member-specific classification
//!   - internal activity
//!   - serialized conversation dispatcher
//!   - typed Talker method            (a message)
//!     or the member's task queue     (an assignment)
//!       - Coder::work
//!   - activity-scoped MCP reads and writes
//! ```
//!
//! [`watcher::Watcher`] rebuilds issue and patch state from their operation histories whenever a
//! relevant reference is fetched. Issue- and patch-specific adapters translate Radicle actions into
//! one normalized model. The classifier then determines whether an operation represents an
//! assignment, mention, reply, or relevant change for the selected member and produces an internal
//! activity. This keeps Radicle's low-level action variants out of consumers.
//!
//! The serialized conversation dispatcher maps each deliverable activity to the typed `mentioned`,
//! `replied`, or `changed` method and acknowledges successful handling before the watcher
//! checkpoints it. Runtime-specific adapters implement these interfaces, and the core does not
//! depend on an agent vendor API. Each callback gets a fresh working copy and returns only success
//! or failure. Repository actions are separate MCP calls, so an agent can perform any number of
//! allowed actions while one unit of work is active.
//!
//! During `run`, the application starts one loopback-only MCP server per started role, scoped to
//! the selected repository and member profile. `start` receives the provider-neutral member
//! profile, its unguessable capability URL, and a preconfigured [`sandbox::AgentSandbox`] before
//! any work is delivered. Manifest allowlists control the exposed read, comment, issue, patch, and
//! review methods, and the split between the roles only withholds what each could not use anyway.
//! Tools are usable only while one delivery or task is active. Reads stay within the repository;
//! writes additionally require a verified task relationship and the existing permissions. Every
//! started agent is stopped before the servers and node shut down.
//!
//! # State and delivery
//!
//! Full history replay is required because later operations depend on earlier assignment and
//! ownership state. Successful action deliveries are stored in SQLite so rescans do not report them
//! twice. Each delivery is recorded after its adapter call succeeds. Adapters receive a stable
//! [`agent::DeliveryId`] in [`agent::Context`]. Its action position distinguishes multiple
//! deliveries from one Radicle operation. Repository writes are journalled under an
//! [`agent::ActivityId`], which names a delivery, chat message, or task, so an interrupted write is
//! recovered by whichever role issued it.
//!
//! # Trust boundaries
//!
//! A member runs under a dedicated Radicle identity and node. [`manifest`] binds the fixed member
//! name to the permitted signing DID and a monotonic generation, preventing a stale or replaced
//! local identity from continuing silently. Private keys and watcher state remain in the isolated
//! member home, while the repository manifest contains only shared identity policy. The application
//! validates this binding after synchronization, before watching activity, and again immediately
//! before every agent-requested write.

pub use crate::{
    application::{Application, Error, Setup, SetupStatus},
    workspace::Error as WorkspaceError,
};

pub mod agent;
mod application;
pub mod builder;
pub mod chat;
pub mod cli;
mod conversation;
mod database;
pub mod home;
pub mod identity;
pub mod manifest;
mod mcp;
pub mod member;
pub mod node;
pub mod provider;
pub mod queue;
pub mod sandbox;
mod shutdown;
mod status;
pub mod watcher;
mod workspace;
