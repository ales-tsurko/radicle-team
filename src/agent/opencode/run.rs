//! What OpenCode writes while it runs, and how that reaches the shared supervision.
//!
//! One delivery is one process, watched by [`Session`]. The plugin reports live progress even when
//! the CLI buffers output or a long tool call has not finished. This module reads output events and
//! explains terminal failures.

use serde::Deserialize;

use crate::{
    agent::{
        opencode::Error,
        session::{self, Outcome, Session},
    },
    sandbox::AgentCommand,
};

/// One line of OpenCode's JSON event stream.
#[derive(Debug, Deserialize)]
pub(crate) struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    error: Option<Failure>,
}

impl session::Event for Event {
    // The plugin reports live progress even while the CLI buffers its output.
    const EVENTS_ARE_ACTIVITY: bool = false;

    fn read(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    // The CLI emits `error` for terminal failures; retries stay in the plugin stream.
    fn failure(&self) -> Option<String> {
        if self.kind != "error" {
            return None;
        }
        Some(session::shorten(&self.error.as_ref()?.message))
    }
}

/// What OpenCode says about a failure it ran into.
#[derive(Debug, Deserialize)]
struct Failure {
    message: String,
}

/// Runs one OpenCode process under the shared supervision.
///
/// # Errors
///
/// Returns this adapter's wording of whatever went wrong, since the messages an operator meets
/// should name the harness they came from.
pub(super) async fn supervise(watched: &Session, command: AgentCommand) -> Result<Outcome, Error> {
    watched
        .run::<Event>(command)
        .await
        .map_err(|ended| match ended {
            session::RunError::Spawn(source) => Error::Sandbox(source),
            session::RunError::Read { role, source } => Error::Output { role, source },
            session::RunError::Fail { status, detail } => Error::Failed { status, detail },
        })
}

#[cfg(test)]
mod tests {
    use crate::agent::{opencode::run::Event, session::Event as _};

    // OpenCode reports what went wrong as an event and then exits non-zero. Without the event the
    // operator is left with an exit code.
    #[test]
    fn reads_a_failure_out_of_an_event_line() {
        let event = Event::read(r#"{"type":"error","error":{"type":"api","message":"Denied"}}"#)
            .expect("an event line should be read");

        assert_eq!(event.kind(), "error");
        assert_eq!(event.failure().as_deref(), Some("Denied"));
    }

    #[test]
    fn does_not_mistake_a_progress_event_for_a_failure() {
        let event = Event::read(r#"{"type":"step_start"}"#).expect("an event line should be read");

        assert_eq!(event.failure(), None);
    }

    // Not every line on the events stream is an event. A line the session cannot read as one is
    // kept as talk instead of being mistaken for a report.
    #[test]
    fn leaves_a_line_that_is_not_an_event_to_the_session() {
        assert!(Event::read("opencode: command not found").is_none());
        assert!(
            Event::read(r#"{"type":"error"}"#)
                .expect("an event without a failure is still an event")
                .failure()
                .is_none()
        );
    }
}
