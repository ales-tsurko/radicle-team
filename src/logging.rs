//! Process-wide tracing configuration.

use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::Mutex,
};

use tracing_subscriber::{
    EnvFilter,
    field::RecordFields,
    filter::LevelFilter,
    fmt::{time::ChronoLocal, writer::BoxMakeWriter},
    layer::SubscriberExt as _,
    util::{SubscriberInitExt as _, TryInitError},
};
use radicle_team::cli::LogBuffer;

pub(crate) struct Logging;

impl Logging {
    pub(crate) fn init(
        level: Option<LevelFilter>,
        logs: Option<LogBuffer>,
        path: Option<&Path>,
    ) -> Result<(), Error> {
        let filter = match level {
            Some(level) => EnvFilter::default().add_directive(level.into()),
            None => EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy(),
        };

        let writer = match logs {
            Some(logs) => BoxMakeWriter::new(logs),
            None => BoxMakeWriter::new(io::stdout),
        };
        let file = path
            .map(|path| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map(Mutex::new)
                    .map_err(|source| Error::File {
                        path: path.into(),
                        source,
                    })
            })
            .transpose()?;
        let output = tracing_subscriber::fmt::layer()
            .with_timer(ChronoLocal::rfc_3339())
            .with_writer(writer);
        let file = file.map(|file| {
            tracing_subscriber::fmt::layer()
                .with_timer(ChronoLocal::rfc_3339())
                .with_ansi(false)
                .fmt_fields(PlainFields)
                .with_writer(file)
        });
        tracing_subscriber::registry()
            .with(filter)
            .with(output)
            .with(file)
            .try_init()?;
        Ok(())
    }

    pub(crate) fn record_failure(path: &Path, error: &impl fmt::Display) -> Result<(), Error> {
        OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "Error: {error}"))
            .map_err(|source| Error::File {
                path: path.into(),
                source,
            })
    }
}

// A distinct formatter keeps file span fields out of the console's ANSI field cache.
#[derive(Debug)]
struct PlainFields;

impl<'writer> tracing_subscriber::fmt::FormatFields<'writer> for PlainFields {
    fn format_fields<R: RecordFields>(
        &self,
        writer: tracing_subscriber::fmt::format::Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        tracing_subscriber::fmt::format::DefaultFields::default().format_fields(writer, fields)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("could not write log file '{path}': {source}")]
    File { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Subscriber(#[from] TryInitError),
}

#[cfg(test)]
mod tests {
    use super::Logging;

    #[test]
    fn file_logging_appends_full_plain_text_and_keeps_span_fields_separate() {
        let root = tempfile::tempdir().expect("log directory");
        let path = root.path().join("member.log");
        std::fs::write(&path, "previous session\n").expect("existing log");
        Logging::init(
            Some(tracing_subscriber::filter::LevelFilter::INFO),
            Some(radicle_team::cli::LogBuffer::default()),
            Some(&path),
        )
        .expect("logging initialized");
        let span = tracing::info_span!("worker", member = "maya");
        let _entered = span.enter();
        for entry in 0..2_100 {
            tracing::info!(entry, "recorded event");
        }
        span.record("member", "nico");
        tracing::info!("updated span fields");
        let contents = std::fs::read_to_string(&path).expect("saved log");
        assert!(contents.starts_with("previous session\n"));
        assert_eq!(contents.lines().count(), 2_102);
        assert!(contents.contains("entry=2099"));
        assert!(contents.contains("worker{member=\"maya\""));
        assert!(contents.contains("member=\"nico\""));
        assert!(
            !contents.contains('\x1b'),
            "files must contain no ANSI codes"
        );
    }
}
