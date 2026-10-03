//! CLI parsing and interactive workflows.

pub use crate::{
    cli::{
        command::{Cli, Error, Mode},
        setup::Error as SetupError,
    },
    status::Error as StatusError,
};

mod command;
pub(crate) mod inspection;
pub(crate) mod provider;
mod setup;
