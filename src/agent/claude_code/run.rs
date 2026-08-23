//! What Claude Code writes while it runs, and how that reaches the shared supervision.
//!
//! One delivery is one process, watched by [`Session`]. Claude Code streams each event as it
//! happens -- a session starting, an assistant turn, a tool result, the final result with what it
//! cost -- so the lines themselves are the progress, and no plugin is needed for the runtime to see
//! the agent working.

use serde::Deserialize;

use crate::{
    agent::{
        claude_code::Error,
        session::{self, Outcome, Session},
    },
    sandbox::AgentCommand,
};

/// One line of Claude Code's JSON event stream.
#[derive(Debug, Deserialize)]
pub(crate) struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    result: Option<String>,
}

impl session::Event for Event {
    // The events arrive as they happen, so every line arriving is one more thing the agent just
    // did. This is the whole reason no plugin stands between them and the runtime.
    const EVENTS_ARE_ACTIVITY: bool = true;

    fn read(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    // Only the final result decides a run failed. A retry it announces along the way is not yet a
    // failure, however bad the news it carries; if the retries never manage to recover, the run
    // ends in a result which says so.
    fn failure(&self) -> Option<String> {
        if self.kind != "result" || self.is_error != Some(true) {
            return None;
        }
        let message = self
            .result
            .clone()
            .filter(|message| !message.is_empty())
            .or_else(|| self.subtype.clone())
            .unwrap_or_else(|| "failed without saying why".to_owned());

        Some(session::shorten(&message))
    }
}

/// Runs one Claude Code process under the shared supervision.
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
    use crate::agent::{claude_code::run::Event, session::Event as _};

    #[test]
    fn reads_a_failure_out_of_a_result_line() {
        let event = Event::read(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Invalid API key. Please run /login."}"#,
        )
        .expect("a result line should be read");

        assert_eq!(event.kind(), "result");
        assert_eq!(
            event.failure().as_deref(),
            Some("Invalid API key. Please run /login."),
            "what the run said about its failure is what the operator is told"
        );
    }

    #[test]
    fn names_a_failure_which_came_without_words() {
        let event = Event::read(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#)
            .expect("a result line should be read");

        assert_eq!(
            event.failure().as_deref(),
            Some("error_max_turns"),
            "the subtype is at least a name to look up"
        );
    }

    // A result which failed and said nothing in its `result` is no different from one which named
    // no `result` at all: the subtype, then the last resort, still have something to say.
    #[test]
    fn falls_back_when_the_result_says_nothing() {
        let empty = Event::read(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":""}"#,
        )
        .expect("a result line should be read");
        let silent =
            Event::read(r#"{"type":"result","is_error":true,"result":""}"#).expect("readable");

        assert_eq!(
            empty.failure().as_deref(),
            Some("error_max_turns"),
            "an empty result carries no words, so the subtype is what is told"
        );
        assert_eq!(
            silent.failure().as_deref(),
            Some("failed without saying why"),
            "nothing anywhere on the line leaves only the last resort"
        );
    }

    #[test]
    fn takes_neither_success_nor_progress_for_failure() {
        let success = Event::read(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
        )
        .expect("a result line should be read");
        let retrying = Event::read(
            r#"{"type":"system","subtype":"api_retry","error":"overloaded","attempt":1}"#,
        )
        .expect("a retry line should be read");
        let working = Event::read(r#"{"type":"assistant","message":{}}"#)
            .expect("an assistant line should be read");

        assert!(success.failure().is_none());
        assert!(
            retrying.failure().is_none(),
            "a retry announced mid-run has not failed; the final result is what decides"
        );
        assert!(working.failure().is_none());
    }

    #[test]
    fn leaves_a_line_that_is_not_an_event_to_the_session() {
        assert!(Event::read("claude: command not found").is_none());
    }
}
