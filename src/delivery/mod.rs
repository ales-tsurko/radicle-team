//! Durable repository notifications and signed delivery receipts.

pub(crate) use crate::delivery::{
    model::{Record, State},
    store::Inbox,
};

mod model;
mod receipt;
pub(crate) mod remote;
pub(crate) mod store;

pub use crate::delivery::error::Error;

mod error;
