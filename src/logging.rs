//! Process-wide tracing configuration.

use tracing_subscriber::{EnvFilter, filter::LevelFilter, util::SubscriberInitExt as _};

pub(crate) struct Logging;

impl Logging {
    pub(crate) fn init() -> Result<(), String> {
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy();

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .finish()
            .try_init()
            .map_err(|error| error.to_string())
    }
}
