//! Read-only Radicle activity watcher.
//!
//! A [`Watcher`](crate::watcher::Watcher) observes one repository as one
//! [`Member`](crate::member::Member), classifies interactions requiring that member's attention,
//! and dispatches them through an [`Agent`](crate::agent::Agent). It reads Radicle state but never
//! changes collaborative objects.
//!
//! # Lifecycle
//!
//! 1. Receive a profile already provisioned and validated by the identity subsystem.
//! 2. Connect to the local node and verify that its identity matches the profile.
//! 3. Replay every issue and patch operation to reconstruct classification state.
//! 4. Dispatch only operations not recorded in the member's local state database.
//! 5. Subscribe to node events and rescan after relevant collaborative-object references are
//!    fetched.
//!
//! Replaying full histories is intentional: assignments and ownership depend on earlier operations.
//! The SQLite state store suppresses duplicate output while still allowing the in-memory
//! classification context to be rebuilt on every scan. Assignment notifications are delayed until
//! replay completes so closed, merged, or subsequently unassigned objects do not produce stale
//! work.
//!
//! # Classification architecture
//!
//! Radicle issue and patch action enums are isolated behind adapter modules. Each adapter
//! implements `From<ActionEnvelope<_>> for NormalizedAction`, translating its concrete actions into
//! shared assignment updates, ownership updates, and ordered relevance signals. A single generic
//! classification context reduces that normalized form into internal activities, which invoke the
//! corresponding typed agent methods.
//!
//! This boundary keeps Radicle-specific variant matching out of the reducer. Adding an upstream
//! action normally changes only its adapter and reuses the existing normalized behavior; only
//! genuinely new semantics require extending the shared classification model.
//!
//! # Structure
//!
//! - `activity` defines the public activity, subject, target, and change model.
//! - `classification` contains the normalized action model and generic reducer.
//! - `issue` and `patch` adapt Radicle action enums into normalized actions.
//! - `runtime` owns scanning and node-event orchestration.
//! - `state` persists processed operation IDs for idempotent rescanning.

pub use crate::watcher::{
    activity::{Change, Comment, Subject, Target},
    runtime::{Error, Watcher},
};

mod activity;
mod classification;
mod issue;
mod patch;
mod runtime;
mod state;
