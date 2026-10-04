//! Shared conversation policy and durable, serialized talker delivery.

pub(crate) use crate::conversation::{
    dispatcher::{Dispatcher, Scoped, supervise},
    policy::{AdmissionError, Author, Triggers, Turn},
};
pub use crate::conversation::dispatcher::Error;

mod dispatcher;
mod policy;
