//! Process-wide tracing configuration.

use tracing_subscriber::{
    EnvFilter, filter::LevelFilter, fmt::time::ChronoLocal, util::SubscriberInitExt as _,
};
use radicle_team::cli::LogBuffer;

pub(crate) struct Logging;

impl Logging {
    pub(crate) fn init(level: Option<LevelFilter>, logs: Option<LogBuffer>) -> Result<(), String> {
        let filter = match level {
            Some(level) => EnvFilter::default().add_directive(level.into()),
            None => EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy(),
        };

        let subscriber = tracing_subscriber::fmt()
            .with_timer(ChronoLocal::rfc_3339())
            .with_env_filter(filter);
        match logs {
            Some(logs) => subscriber
                .with_ansi(false)
                .with_writer(logs)
                .finish()
                .try_init(),
            None => subscriber.finish().try_init(),
        }
        .map_err(|error| error.to_string())
    }
}
