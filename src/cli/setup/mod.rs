//! Interactive setup workflow.

pub use crate::cli::setup::wizard::Error;
pub(crate) use crate::cli::setup::{
    checkout::RepositoryCheckout,
    manifest::{ManifestEntry, ManifestUpdate},
    sandbox::SandboxSetup,
    security::{SecurityManifest, SecuritySetup},
    wizard::SetupWizard,
};

mod checkout;
mod manifest;
mod sandbox;
mod security;
mod wizard;

#[cfg(test)]
mod tests;
