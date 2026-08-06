//! Radicle identity ownership and key management.
//!
//! This module is the only part of the library that provisions or loads a member's signing
//! identity. It keeps the fixed character separate from its Radicle DID and holds the member-home
//! lock that prevents two local processes from using the same identity.
//!
//! [`Manager`] resolves a [`Provider`] into an [`Identity`]. [`GenerationStore`] prevents an older
//! member identity binding from silently replacing one already observed locally. Project
//! configuration and binding validation live in [`crate::manifest`].

pub use crate::identity::{
    credentials::{CredentialId, CredentialStore, Error as CredentialError, SystemCredentialStore},
    generation::{Error as GenerationError, GenerationStore},
    lock::{Error as LockError, MemberLock},
    permissions::Error as PermissionError,
    provider::{Error as ProviderError, Identity, KeyProtection, Manager, Provider},
};

mod credentials;
mod generation;
mod lock;
pub(crate) mod permissions;
mod provider;
