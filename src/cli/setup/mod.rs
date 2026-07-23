//! Interactive setup workflow.

pub use crate::cli::setup::wizard::Error;
pub(super) use crate::cli::setup::{
    checkout::RepositoryCheckout, manifest::ManifestEntry, wizard::SetupWizard,
};

mod checkout;
mod manifest;
mod wizard;

#[cfg(all(test, not(miri)))]
mod tests;
