//! Process-wide tracing configuration.

use tracing_subscriber::{EnvFilter, filter::LevelFilter, util::SubscriberInitExt as _};

pub(crate) struct Logging;

impl Logging {
    pub(crate) fn init(level: Option<LevelFilter>) -> Result<(), String> {
        let filter = match level {
            Some(level) => EnvFilter::default().add_directive(level.into()),
            None => EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy(),
        };

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .finish()
            .try_init()
            .map_err(|error| error.to_string())
    }
}
