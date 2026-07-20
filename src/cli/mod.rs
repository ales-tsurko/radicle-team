//! CLI parsing and interactive workflows.

pub use crate::cli::{
    command::{Cli, Error, Mode},
    setup::Error as SetupError,
};

mod command;
mod setup;
