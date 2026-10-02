//! One agent process, watched while it runs.
//!
//! Every adapter delivers by starting one process and is responsible for reading what that process
//! writes. What they all share -- stopping a run in flight, telling a stopped run from one asked to
//! preserve its work, noticing when an agent last did anything, keeping enough of what it wrote to
//! explain a failure -- lives here, so the differences between adapters are only ever about how
//! their harness talks.
//!
//! The seam is [`Event`]. Each adapter teaches this module how to read one line of its harness's
//! own event stream, and whether receiving one counts as the agent doing something. Everything else
//! is common, which is why a stalled permission prompt looks the same from out here whichever agent
//! application caused it.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime},
};

use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, BufReader},
    sync::Notify,
};

use crate::{
    agent::Role,
    sandbox::{AgentChild, AgentCommand},
};

// How much of what a run wrote is kept to explain a failure with. Enough to see what went wrong,
// and bounded, because a process which writes without stopping must not exhaust this one's memory.
const KEPT_LINES: usize = 10;
const KEPT_WIDTH: usize = 400;

/// One line of the agent application's own output, read as what it says rather than as text.
///
/// Adapters implement this for their harness's event shape. A line which does not parse is not an
/// error: a harness talking about itself instead of reporting is kept for the failure message,
/// since a run which cannot produce events fails exactly there.
pub(crate) trait Event: Sized {
    /// Whether receiving an event counts as the agent doing something.
    ///
    /// Some harnesses stream each event as it happens, so every line arriving is one more thing the
    /// agent just did. Others write their events when the run is over, so there a line is history
    /// rather than activity, and progress has to reach this module some other way -- a report file,
    /// say, whose changing mtime [`Progress`] watches.
    const EVENTS_ARE_ACTIVITY: bool;

    /// Reads one line as an event, or returns nothing when the harness was talking instead of
    /// reporting.
    fn read(line: &str) -> Option<Self>;

    /// Returns what the event is called, for the log.
    fn kind(&self) -> &str;

    /// Returns what went wrong, when the event carries a failure.
    ///
    /// A failure returned here ends the run as failed whatever the process's exit status says: some
    /// harnesses report a failure as an event and still exit zero, and a delivery which looks
    /// successful while publishing nothing is the worst way to fail.
    fn failure(&self) -> Option<String>;
}

/// The running side of one role, shared with the runtime that supervises it.
///
/// Held apart from the adapter because the runtime watches an agent while that same adapter is busy
/// handling the delivery, so the two cannot both borrow it.
#[derive(Debug)]
pub(crate) struct Session {
    role: Role,
    // Where a plugin reports what the agent is doing, for adapters whose harness needs one to make
    // itself visible. Known once the role has been started; absent otherwise, and every read of it
    // then falls back to what was seen on the streams alone.
    report: OnceLock<PathBuf>,
    progress: Mutex<Progress>,
    // Present only while a run is in flight, and fresh for each one, so a stop asked for during
    // one run cannot end the next.
    stop: Mutex<Option<Arc<Notify>>>,
    interrupted: AtomicBool,
}

impl Session {
    /// Creates the shared state for one role's runs.
    pub(crate) fn new(role: Role) -> Self {
        Self {
            role,
            report: OnceLock::new(),
            progress: Mutex::new(Progress::default()),
            stop: Mutex::new(None),
            interrupted: AtomicBool::new(false),
        }
    }

    /// Says where a plugin reports this role's progress, once the role has been started.
    pub(crate) fn reports_in(&self, report: PathBuf) {
        let _first = self.report.set(report);
    }

    /// Runs `command` to completion, unless it is stopped first.
    ///
    /// One run at a time, which is what the runtime gives a role: each agent serialises its own
    /// work, so a role is at most one delivery or one task at once.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be started, its output cannot be read, or it ends
    /// in failure. Which error that is belongs to the adapter, which words failures the way its
    /// operator knows them.
    pub(crate) async fn run<P: Event>(&self, command: AgentCommand) -> Result<Outcome, RunError> {
        let stop = self.begin();
        let outcome = self.drive::<P>(command, &stop).await;
        self.end();

        outcome
    }

    /// Asks the running process to stop and to have its work preserved.
    ///
    /// What preserves it is what happens next, not this: the working copy keeps whatever the agent
    /// has written, and the adapter asks a fresh run to commit it.
    pub(crate) fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        self.stop();
    }

    /// Returns whether the run which was stopped had been asked to stop by somebody.
    pub(crate) fn was_interrupted(&self) -> bool {
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

    async fn drive<P: Event>(
        &self,
        command: AgentCommand,
        stop: &Notify,
    ) -> Result<Outcome, RunError> {
        tracing::debug!(role = %self.role, ?command, "starting a supervised agent run");
        let child = command.spawn().await.map_err(RunError::Spawn)?;
        tokio::select! {
            finished = self.finish::<P>(child) => finished,
            // Dropping the running future here drops the child with it, which is what ends the
            // process: the sandbox starts every agent to be killed when its handle goes.
            () = stop.notified() => {
                tracing::info!(role = %self.role, "stopped a running agent process");

                Ok(Outcome::Stopped)
            }
        }
    }

    // Waits for the process to end, reading both its streams while it runs.
    //
    // Both are drained at once. Reading one to its end first would stop the process the moment it
    // filled the other's pipe, and neither side would move again.
    async fn finish<P: Event>(&self, mut child: AgentChild) -> Result<Outcome, RunError> {
        let (events, diagnostics) = (child.stdout(), child.stderr());
        let (events, diagnostics, status) = tokio::join!(
            self.read::<P>(events, Stream::Events),
            self.read::<P>(diagnostics, Stream::Diagnostics),
            child.wait()
        );
        let (events, diagnostics, status) = (events?, diagnostics?, status?);

        concluded(&events, &diagnostics, status)
    }

    // Reads one of the process's streams to its end, keeping what explains a failure.
    async fn read<P: Event>(
        &self,
        stream: Option<impl AsyncRead + Unpin>,
        kind: Stream,
    ) -> Result<Report, RunError> {
        let mut report = Report::default();
        let Some(stream) = stream else {
            return Ok(report);
        };
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = lines.next_line().await.map_err(|source| RunError::Read {
                role: self.role,
                source,
            })?;
            let Some(line) = line else {
                return Ok(report);
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match kind {
                Stream::Events => match P::read(line) {
                    Some(event) => {
                        tracing::debug!(
                            role = %self.role,
                            event = event.kind(),
                            "the agent reported an event"
                        );
                        if let Some(failure) = event.failure() {
                            report.fail(failure);
                        }
                        if P::EVENTS_ARE_ACTIVITY {
                            self.saw();
                        }
                    }
                    // Not an event, so it is the harness talking rather than reporting. Kept for
                    // the failure message, since a run which cannot produce events fails here.
                    None => report.remember(line),
                },
                Stream::Diagnostics => {
                    tracing::debug!(
                        role = %self.role,
                        line = shorten(line),
                        "the agent wrote a diagnostic"
                    );
                    report.remember(line);
                }
            }
        }
    }
}

// Decides how a run ended, from what the agent reported and how the process exited.
//
// What the agent reported decides first: a harness which reports a failure in its own events has
// failed the run, whatever exit status it left behind. Some exit zero on a failure they reported,
// and the exit status alone would call such a run a success -- a delivery which publishes nothing
// and never says why.
fn concluded(
    events: &Report,
    diagnostics: &Report,
    status: ExitStatus,
) -> Result<Outcome, RunError> {
    if let Some(detail) = events.failed() {
        return Err(RunError::Fail { status, detail });
    }
    if status.success() {
        return Ok(Outcome::Finished);
    }

    Err(RunError::Fail {
        status,
        detail: events.explain().unwrap_or_else(|| diagnostics.tail()),
    })
}

impl Session {
    /// Reports when the agent last did something.
    ///
    /// For a harness whose events are activity, that is whatever the streams brought in. Otherwise
    /// the runtime asks every ten seconds, and the report file a plugin writes is read on the same
    /// beat rather than watched: what matters is when it last changed, not what it says. One more
    /// line means the agent is still working, and which line it is only matters when the run fails.
    pub(crate) fn last_progress(&self) -> Option<Instant> {
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(report) = self.report.get() {
            progress.reported(report);
        }

        progress.at
    }

    /// Force-stops whatever the agent is doing, whether it agreed to or not.
    ///
    /// Weaker than an adapter's `shutdown`: this ends one delivery, and the agent stays running
    /// for the next one.
    pub(crate) async fn abort(&self) -> Result<(), crate::agent::Error> {
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
pub(crate) enum Outcome {
    /// The agent finished the delivery and exited.
    Finished,
    /// The run was stopped before the agent was done with it.
    Stopped,
}

/// Why a supervised run failed, before the adapter words it.
///
/// The messages an operator sees belong to the adapter, which names its own harness; this carries
/// the facts across the seam untouched.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// Starting or running a sandboxed process failed.
    #[error(transparent)]
    Spawn(#[from] crate::sandbox::Error),
    /// Reading what the agent wrote failed.
    #[error("could not read what the {role} role's agent wrote: {source}")]
    Read {
        /// Role whose output could not be read.
        role: Role,
        /// Underlying stream error.
        source: std::io::Error,
    },
    /// A run ended in failure. Some harnesses exit zero after reporting one, so the status is
    /// context rather than the verdict; the failure is in what was said.
    #[error("the agent failed (ended with {status}): {detail}")]
    Fail {
        /// How the process exited.
        status: ExitStatus,
        /// What it reported about the failure.
        detail: String,
    },
}

/// Which of a run's two streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// What the agent did, as the harness's own JSON events.
    Events,
    /// Whatever the harness wrote outside those, which is where a failure to start shows up.
    Diagnostics,
}

/// What a run wrote, as much of it as is worth keeping.
#[derive(Debug, Default)]
struct Report {
    failure: Option<String>,
    tail: VecDeque<String>,
}

impl Report {
    // Keeps the first failure reported, which is the one that stopped the run being useful.
    fn fail(&mut self, failure: String) {
        self.failure.get_or_insert(failure);
    }

    // Returns the failure an event reported, if one did.
    fn failed(&self) -> Option<String> {
        self.failure.clone()
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
        self.tail.push_back(shorten(line));
    }
}

/// Shortens a line to what is worth reading, on a character boundary.
pub(crate) fn shorten(line: &str) -> String {
    match line.char_indices().nth(KEPT_WIDTH) {
        Some((end, _character)) => format!("{}…", line.split_at(end).0),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::{
        Role,
        claude_code::run::Event as ClaudeEvent,
        opencode::run::Event,
        session::{self, KEPT_LINES, KEPT_WIDTH, Outcome, Session, Stream},
    };

    // The runtime stops an agent which has shown nothing for fifteen minutes. From outside the
    // process there is nothing to see, so what counts as showing something is what a plugin wrote
    // down for a harness which does not stream its events.
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
            .join("session-progress");
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

    // For a harness whose events arrive as they happen, one line arriving is one more thing the
    // agent just did: no plugin, and nothing to watch on disk. For a harness which writes its
    // events when the run is over, the same line is history, and progress has to come from the
    // report file instead -- mistaking one for the other either sees nothing or calls a hung agent
    // busy.
    #[tokio::test]
    async fn takes_progress_from_the_events_which_count_as_activity() {
        let streamed = Session::new(Role::Talker);
        assert_eq!(
            streamed.last_progress(),
            None,
            "nothing has happened in a role nobody started"
        );
        streamed
            .read::<ClaudeEvent>(
                Some(&b"{\"type\":\"assistant\",\"message\":{}}\n"[..]),
                Stream::Events,
            )
            .await
            .expect("events should be read");
        assert!(
            streamed.last_progress().is_some(),
            "one event from Claude Code's stream is the agent doing something"
        );

        let reported_by_plugin = Session::new(Role::Coder);
        reported_by_plugin
            .read::<Event>(
                Some(&br#"{"type":"error","error":{"type":"api","message":"Overloaded"}}"#[..]),
                Stream::Events,
            )
            .await
            .expect("events should be read");
        assert_eq!(
            reported_by_plugin.last_progress(),
            None,
            "OpenCode progress comes from its live plugin stream, even when CLI output is buffered"
        );
    }

    // A run which reports what went wrong as an event and then exits non-zero leaves the operator
    // with more than an exit code.
    #[tokio::test]
    async fn explains_a_failure_with_what_the_agent_said_about_it() {
        let session = Session::new(Role::Talker);

        let report = session
            .read::<Event>(
                Some(
                    &br#"{"type":"error","error":{"type":"api","message":"Authentication Fails"}}"#
                        [..],
                ),
                Stream::Events,
            )
            .await
            .expect("events should be read");

        assert_eq!(report.explain().as_deref(), Some("Authentication Fails"));
    }

    // A harness which reports a failure in its own events and exits zero -- Claude Code does this
    // for a model the account cannot reach -- fails the run all the same. Read the exit status
    // alone and such a delivery would look like a success: nothing published, and nothing saying
    // why.
    #[tokio::test]
    async fn takes_a_reported_failure_over_an_exit_of_zero() {
        let session = Session::new(Role::Talker);
        let events = session
            .read::<ClaudeEvent>(
                Some(
                    &br#"{"type":"result","subtype":"success","is_error":true,"result":"There's an issue with the selected model"}"#
                        [..],
                ),
                Stream::Events,
            )
            .await
            .expect("events should be read");
        let diagnostics = session
            .read::<ClaudeEvent>(Some(&b""[..]), Stream::Diagnostics)
            .await
            .expect("diagnostics should be read");

        let error = session::concluded(&events, &diagnostics, exited(0))
            .expect_err("a failure the agent reported is a failure whatever the exit said");
        assert!(
            error.to_string().contains("selected model"),
            "the operator is told what went wrong rather than nothing at all: {error}"
        );

        // The other side of the same coin: no reported failure and a zero exit is a success,
        // whatever else the run wrote.
        let quiet = session
            .read::<ClaudeEvent>(
                Some(&br#"{"type":"result","subtype":"success","is_error":false}"#[..]),
                Stream::Events,
            )
            .await
            .expect("events should be read");
        assert!(
            matches!(
                session::concluded(&quiet, &diagnostics, exited(0)),
                Ok(Outcome::Finished)
            ),
            "a run which reported no failure and exited zero finished"
        );
    }

    // OpenCode terminal errors must fail the delivery even if the CLI exits zero.
    #[tokio::test]
    async fn fails_an_opencode_run_which_reported_a_failure_and_exited_zero() {
        let failure = r#"{"type":"error","error":{"type":"api","message":"Denied"}}"#;
        let session = Session::new(Role::Coder);
        let events = session
            .read::<Event>(Some(failure.as_bytes()), Stream::Events)
            .await
            .expect("events should be read");
        let diagnostics = session
            .read::<Event>(Some(&b""[..]), Stream::Diagnostics)
            .await
            .expect("diagnostics should be read");

        let error = session::concluded(&events, &diagnostics, exited(0))
            .expect_err("a failure the agent reported ends the run however it exited");
        assert!(
            error.to_string().contains("Denied"),
            "the operator is told why, not merely that: {error}"
        );
    }

    // A run which cannot produce events at all -- a missing binary, a sandbox which refused --
    // fails with whatever it managed to write instead.
    #[tokio::test]
    async fn falls_back_to_what_a_run_wrote_outside_its_events() {
        let session = Session::new(Role::Talker);

        let report = session
            .read::<Event>(
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
            .read::<Event>(Some(written.as_bytes()), Stream::Diagnostics)
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
    async fn separates_a_task_asked_to_stop_from_one_which_was_abandoned() {
        let session = Session::new(Role::Coder);
        let _stop = session.begin();

        assert!(!session.was_interrupted());
        session.abort().await.expect("aborting should be reported");
        assert!(
            !session.was_interrupted(),
            "a run the runtime gave up on has nothing to preserve"
        );

        session.interrupt();
        assert!(session.was_interrupted());

        let _next = session.begin();
        assert!(
            !session.was_interrupted(),
            "a stop asked for during one task must not follow the next one"
        );
    }

    // A process's exit, as the sandbox would report it.
    fn exited(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }
}
