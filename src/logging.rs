//! Process-wide tracing configuration.

use tracing_subscriber::{EnvFilter, filter::LevelFilter, util::SubscriberInitExt as _};
use radicle_team::cli::Mode;

pub(crate) struct Logging;

impl Logging {
    pub(crate) fn init(mode: Mode) -> Result<(), String> {
        let level = if mode == Mode::Run {
            LevelFilter::INFO
        } else {
            LevelFilter::WARN
        };
        let filter = EnvFilter::builder()
            .with_default_directive(level.into())
            .from_env_lossy();

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .finish()
            .try_init()
            .map_err(|error| error.to_string())
    }
}
