//! Runtime-owned activity snapshots and read-only member inspection.

pub use crate::status::error::Error;
pub(crate) use crate::status::{
    inspection::Inspection,
    model::{Activity, Phase, Snapshot},
    store::Reporter,
};

mod error;
mod inspection;
mod model;
mod store;

#[cfg(test)]
mod tests;
