//! CLI parsing and interactive workflows.

pub use crate::{
    cli::{
        command::{Cli, Error, Mode},
        dashboard::LogBuffer,
        setup::Error as SetupError,
    },
    status::Error as StatusError,
};

mod command;
mod dashboard;
mod display;
mod inbox;
pub(crate) mod inspection;
pub(crate) mod provider;
mod setup;
