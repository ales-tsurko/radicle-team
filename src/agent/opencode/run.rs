//! What OpenCode writes while it runs, and how that reaches the shared supervision.
//!
//! One delivery is one process, watched by [`Session`]. OpenCode writes its JSON events once a
//! turn is over rather than as it happens, which is why progress reaches the session through the
//! plugin's report file instead of through these lines. What this adapter adds is only how to read
//! one event line and how to word a failure.

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
    // The events arrive when a run is over, so a line here is history rather than activity. What
    // the agent is doing meanwhile is what the plugin reports.
    const EVENTS_ARE_ACTIVITY: bool = false;

    fn read(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    // An error line here is terminal. OpenCode writes its events once a run is over rather than
    // as it happens, so there are no transient ones to ride out mid-stream -- which is what lets
    // the session fail a run on what it reported even when the exit status says otherwise.
    fn failure(&self) -> Option<String> {
        let failure = self.error.as_ref()?;
        let message = failure
            .data
            .as_ref()
            .and_then(|data| data.message.clone())
            .or_else(|| failure.name.clone())?;

        Some(session::shorten(&message))
    }
}

/// What OpenCode says about a failure it ran into.
#[derive(Debug, Deserialize)]
struct Failure {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    data: Option<FailureData>,
}

#[derive(Debug, Deserialize)]
struct FailureData {
    #[serde(default)]
    message: Option<String>,
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
        let event = Event::read(
            r#"{"type":"error","error":{"name":"APIError","data":{"message":"Denied"}}}"#,
        )
        .expect("an event line should be read");

        assert_eq!(event.kind(), "error");
        assert_eq!(
            event.failure().as_deref(),
            Some("Denied"),
            "the message says why the run failed; the error's own name is the fallback"
        );
    }

    // A named failure without a message still has something to say.
    #[test]
    fn falls_back_to_what_the_failure_is_called() {
        let event = Event::read(r#"{"type":"error","error":{"name":"Overloaded"}}"#)
            .expect("an event line should be read");

        assert_eq!(event.failure().as_deref(), Some("Overloaded"));
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
