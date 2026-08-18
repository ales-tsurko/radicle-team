//! One OpenCode process, watched while it runs.
//!
//! A delivery is one process. Nothing is resumed between deliveries: what an agent needs to know
//! about the issue, patch, or job it is answering it reads through MCP, which is the record
//! everybody else reads too.
//!
//! What the agent is doing comes from the plugin the adapter installs, which writes a line per
//! event into the role's own harness home. From outside the process a working agent and a stuck one
//! look the same, and OpenCode is the only side which knows the difference. Stopping is the same
//! act either way -- the process is ended and the run reports that it was stopped -- and what that
//! means is decided by whoever asked: the runtime aborting a silent agent, or a person cancelling a
//! job.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime},
};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, BufReader},
    sync::Notify,
};

use crate::{
    agent::{self, Role, Supervisor, opencode::Error},
    sandbox::AgentCommand,
};

// How much of what a run wrote is kept to explain a failure with. Enough to see what went wrong,
// and bounded, because a process which writes without stopping must not exhaust this one's memory.
const KEPT_LINES: usize = 10;
const KEPT_WIDTH: usize = 400;

/// The running side of one role, shared with the runtime that supervises it.
///
/// Held apart from the adapter because the runtime watches an agent while that same adapter is busy
/// handling the delivery, so the two cannot both borrow it.
#[derive(Debug)]
pub(super) struct Session {
    role: Role,
    // Where the plugin writes what the agent is doing. Known once the role has been started.
    report: OnceLock<PathBuf>,
    progress: Mutex<Progress>,
    // Present only while a run is in flight, and fresh for each one, so a stop asked for during
    // one run cannot end the next.
    stop: Mutex<Option<Arc<Notify>>>,
    interrupted: AtomicBool,
}

impl Session {
    /// Creates the shared state for one role's runs.
    pub(super) fn new(role: Role) -> Self {
        Self {
            role,
            report: OnceLock::new(),
            progress: Mutex::new(Progress::default()),
            stop: Mutex::new(None),
            interrupted: AtomicBool::new(false),
        }
    }

    /// Says where the plugin reports this role's progress, once the role has been started.
    pub(super) fn reports_in(&self, report: PathBuf) {
        let _first = self.report.set(report);
    }

    /// Runs `command` to completion, unless it is stopped first.
    ///
    /// One run at a time, which is what the runtime gives a role: each agent serialises its own
    /// work, so a role is at most one delivery or one job at once.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be started, its output cannot be read, or it ends
    /// in failure.
    pub(super) async fn run(&self, command: AgentCommand) -> Result<Outcome, Error> {
        let stop = self.begin();
        let outcome = self.drive(command, &stop).await;
        self.end();

        outcome
    }

    /// Asks the running process to stop and to have its work preserved.
    ///
    /// What preserves it is what happens next, not this: the working copy keeps whatever the agent
    /// has written, and the adapter asks a fresh run to commit it.
    pub(super) fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        self.stop();
    }

    /// Returns whether the run which was stopped had been asked to stop by somebody.
    pub(super) fn was_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    /// Records that the agent did something just now.
    fn saw(&self) {
        self.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seen();
    }

    // Empties the report, so what the last run did is not read as this one making progress.
    fn opened(&self) {
        let Some(report) = self.report.get() else {
            return;
        };
        if let Err(error) = fs::write(report, "") {
            tracing::warn!(
                role = %self.role,
                path = %report.display(),
                %error,
                "could not clear the file the agent reports its progress in"
            );
        }
    }

    // Ends whatever is running, whether it agreed to or not.
    fn stop(&self) {
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            // A permit rather than a wake-up, so a stop which arrives before the run waits for one
            // still ends it.
            stop.notify_one();
        }
    }

    // Opens a run and returns what ends it.
    fn begin(&self) -> Arc<Notify> {
        let stop = Arc::new(Notify::new());
        self.interrupted.store(false, Ordering::SeqCst);
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&stop));
        self.opened();
        self.saw();

        stop
    }

    fn end(&self) {
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    async fn drive(&self, command: AgentCommand, stop: &Notify) -> Result<Outcome, Error> {
        tracing::debug!(role = %self.role, ?command, "starting an OpenCode run");
        let child = command.spawn().await?;
        tokio::select! {
            finished = self.finish(child) => finished,
            // Dropping the running future here drops the child with it, which is what ends the
            // process: the sandbox starts every agent to be killed when its handle goes.
            () = stop.notified() => {
                tracing::info!(role = %self.role, "stopped a running OpenCode process");

                Ok(Outcome::Stopped)
            }
        }
    }

    // Waits for the process to end, reading both its streams while it runs.
    //
    // Both are drained at once. Reading one to its end first would stop the process the moment it
    // filled the other's pipe, and neither side would move again.
    async fn finish(&self, mut child: crate::sandbox::AgentChild) -> Result<Outcome, Error> {
        let (events, diagnostics) = (child.stdout(), child.stderr());
        let (events, diagnostics, status) = tokio::join!(
            self.read(events, Stream::Events),
            self.read(diagnostics, Stream::Diagnostics),
            child.wait()
        );
        let (events, diagnostics, status) = (events?, diagnostics?, status?);
        if status.success() {
            return Ok(Outcome::Finished);
        }

        Err(Error::Failed {
            status,
            detail: events.explain().unwrap_or_else(|| diagnostics.tail()),
        })
    }

    // Reads one of the process's streams to its end, keeping what explains a failure.
    async fn read(
        &self,
        stream: Option<impl AsyncRead + Unpin>,
        kind: Stream,
    ) -> Result<Report, Error> {
        let mut report = Report::default();
        let Some(stream) = stream else {
            return Ok(report);
        };
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = lines.next_line().await.map_err(|source| Error::Output {
                role: self.role,
                source,
            })?;
            let Some(line) = line else {
                return Ok(report);
            };
            report.keep(kind, self.role, &line);
        }
    }
}

#[async_trait]
impl Supervisor for Session {
    /// Reports when the agent last did something, as the plugin last wrote it down.
    ///
    /// The runtime asks every ten seconds, so the file is read on the same beat rather than
    /// watched. What is read is when it last changed, not what it says: one more line means the
    /// agent is still working, and which line it is only matters when the run fails.
    fn last_progress(&self) -> Option<Instant> {
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(report) = self.report.get() {
            progress.reported(report);
        }

        progress.at
    }

    async fn abort(&self) -> Result<(), agent::Error> {
        self.stop();

        Ok(())
    }
}

/// When the agent was last seen doing something.
#[derive(Debug, Default)]
struct Progress {
    at: Option<Instant>,
    // What the report's modification time was when it was last read. A file which has changed
    // since then is an agent which has done something since then.
    written: Option<SystemTime>,
}

impl Progress {
    fn seen(&mut self) {
        self.at = Some(Instant::now());
    }

    fn reported(&mut self, report: &Path) {
        let Ok(written) = fs::metadata(report).and_then(|report| report.modified()) else {
            return;
        };
        if self.written != Some(written) {
            self.written = Some(written);
            self.seen();
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// OpenCode finished the delivery and exited.
    Finished,
    /// The run was stopped before OpenCode was done with it.
    Stopped,
}

/// Which of a run's two streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// What the agent did, as OpenCode's own JSON events.
    Events,
    /// Whatever OpenCode wrote outside those, which is where a failure to start shows up.
    Diagnostics,
}

/// What a run wrote, as much of it as is worth keeping.
#[derive(Debug, Default)]
struct Report {
    failure: Option<String>,
    tail: VecDeque<String>,
}

impl Report {
    // Keeps what this line adds, and logs it.
    fn keep(&mut self, kind: Stream, role: Role, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match kind {
            Stream::Events => match serde_json::from_str::<Event>(line) {
                Ok(event) => {
                    tracing::debug!(%role, event = event.kind, "OpenCode reported an event");
                    if let Some(failure) = event.failure() {
                        self.failure.get_or_insert(failure);
                    }
                }
                // Not an event, so it is OpenCode talking rather than reporting. Kept for the
                // failure message, since a run which cannot produce events fails here.
                Err(_error) => self.remember(line),
            },
            Stream::Diagnostics => {
                tracing::debug!(%role, line = Self::short(line), "OpenCode wrote a diagnostic");
                self.remember(line);
            }
        }
    }

    // Returns the failure this run reported, if it reported one.
    fn explain(&self) -> Option<String> {
        self.failure.clone().or_else(|| match self.tail.is_empty() {
            true => None,
            false => Some(self.tail()),
        })
    }

    // Returns the last of what was written, for a failure which explained itself no other way.
    fn tail(&self) -> String {
        Vec::from_iter(self.tail.iter().cloned()).join("; ")
    }

    fn remember(&mut self, line: &str) {
        if self.tail.len() == KEPT_LINES {
            let _oldest = self.tail.pop_front();
        }
        self.tail.push_back(Self::short(line));
    }

    // Shortens a line to what is worth reading, on a character boundary.
    fn short(line: &str) -> String {
        match line.char_indices().nth(KEPT_WIDTH) {
            Some((end, _character)) => format!("{}…", line.split_at(end).0),
            None => line.to_owned(),
        }
    }
}

/// One line of OpenCode's JSON event stream.
#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    error: Option<Failure>,
}

impl Event {
    fn failure(&self) -> Option<String> {
        let failure = self.error.as_ref()?;
        let message = failure
            .data
            .as_ref()
            .and_then(|data| data.message.clone())
            .or_else(|| failure.name.clone())?;

        Some(Report::short(&message))
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

// Excluded from Miri: every test here builds a Tokio runtime, and Miri cannot make the `kqueue`
// call its IO driver starts with.
#[cfg(all(test, not(miri)))]
mod tests {
    use crate::agent::{
        Role, Supervisor as _,
        opencode::run::{KEPT_LINES, KEPT_WIDTH, Session, Stream},
    };

    // The runtime stops an agent which has shown nothing for fifteen minutes. From outside the
    // process there is nothing to see, so what counts as showing something is what the plugin
    // wrote down.
    #[cfg(not(miri))]
    #[tokio::test]
    async fn takes_progress_from_what_the_plugin_reports() {
        let session = Session::new(Role::Coder);
        assert_eq!(
            session.last_progress(),
            None,
            "a role which has not started has shown nothing"
        );

        let report = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("opencode-progress");
        let _removed = std::fs::remove_dir_all(&report);
        std::fs::create_dir_all(&report).expect("test directory should be created");
        let report = report.join("progress.jsonl");
        std::fs::write(&report, "{\"kind\":\"started\"}\n").expect("report should be written");
        session.reports_in(report.clone());

        let first = session
            .last_progress()
            .expect("a written report is an agent working");
        assert_eq!(
            session.last_progress(),
            Some(first),
            "a report nobody added to says the agent has done nothing since"
        );

        std::fs::write(&report, "{\"kind\":\"started\"}\n{\"kind\":\"tool\"}\n")
            .expect("report should be added to");
        // Polled rather than slept through, because how finely a filesystem records a change is
        // the filesystem's business.
        let mut moved = None;
        for _attempt in 0..100 {
            match session.last_progress() {
                Some(seen) if seen > first => {
                    moved = Some(seen);
                    break;
                }
                _unchanged => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }

        assert!(
            moved.is_some(),
            "another line in the report is the agent doing something else"
        );
    }

    // OpenCode reports what went wrong as an event and then exits non-zero. Without the event the
    // operator is left with an exit code.
    #[tokio::test]
    async fn explains_a_failure_with_what_opencode_said_about_it() {
        let session = Session::new(Role::Talker);

        let report = session
            .read(
                Some(
                    &br#"{"type":"error","error":{"name":"APIError","data":{"message":"Authentication Fails"}}}"#
                        [..],
                ),
                Stream::Events,
            )
            .await
            .expect("events should be read");

        assert_eq!(report.explain().as_deref(), Some("Authentication Fails"));
    }

    // A run which cannot produce events at all -- a missing binary, a sandbox which refused --
    // fails with whatever it managed to write instead.
    #[tokio::test]
    async fn falls_back_to_what_a_run_wrote_outside_its_events() {
        let session = Session::new(Role::Talker);

        let report = session
            .read(
                Some(&b"opencode: command not found\n"[..]),
                Stream::Diagnostics,
            )
            .await
            .expect("diagnostics should be read");

        assert_eq!(
            report.explain().as_deref(),
            Some("opencode: command not found")
        );
    }

    // A process which writes without stopping must not exhaust the one supervising it.
    #[tokio::test]
    async fn keeps_a_bounded_amount_of_what_a_run_wrote() {
        let session = Session::new(Role::Coder);
        let written = (0..KEPT_LINES * 3)
            .map(|line| format!("line {line} {}", "x".repeat(KEPT_WIDTH * 2)))
            .collect::<Vec<_>>()
            .join("\n");

        let report = session
            .read(Some(written.as_bytes()), Stream::Diagnostics)
            .await
            .expect("diagnostics should be read");

        let explained = report.explain().expect("a failure should be explainable");
        assert!(
            explained.chars().count() <= KEPT_LINES * (KEPT_WIDTH + 32),
            "kept {} characters of a run which wrote {}",
            explained.chars().count(),
            written.chars().count()
        );
        assert!(
            explained.contains(&format!("line {}", KEPT_LINES * 3 - 1)),
            "what a failing run wrote last is what says why it failed"
        );
    }

    // Being aborted for silence and being asked to stop end the same process, and only the second
    // is followed by asking the agent to commit what it reached.
    #[tokio::test]
    async fn separates_a_job_asked_to_stop_from_one_which_was_abandoned() {
        let session = Session::new(Role::Coder);
        let _stop = session.begin();

        assert!(!session.was_interrupted());
        crate::agent::Supervisor::abort(&session)
            .await
            .expect("aborting should be reported");
        assert!(
            !session.was_interrupted(),
            "a run the runtime gave up on has nothing to preserve"
        );

        session.interrupt();
        assert!(session.was_interrupted());

        let _next = session.begin();
        assert!(
            !session.was_interrupted(),
            "a stop asked for during one job must not follow the next one"
        );
    }
}
