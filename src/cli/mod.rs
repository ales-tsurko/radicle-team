//! CLI parsing and interactive workflows.

pub use crate::{
    cli::{
        command::{Cli, Error, Mode},
        setup::Error as SetupError,
    },
    status::Error as StatusError,
};

mod command;
mod inbox;
pub(crate) mod inspection;
pub(crate) mod provider;
mod setup;
