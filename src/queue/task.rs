//! Tasks, submissions, and minimal completed-request receipts.

use std::str::FromStr as _;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    agent::Source,
    manifest::QueuePosition,
    queue::{ArtifactRole, TaskContext},
    watcher::Subject,
};

/// A durable task queued for one member's building role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Database identifier, assigned when the task is recorded.
    pub id: i64,
    /// What to do, in enough detail to begin without rereading the discussion.
    pub brief: String,
    /// Place the request came from.
    pub source: Source,
    /// Where the task joined the queue.
    pub position: QueuePosition,
    /// Specification, related artifacts, pinned messages, and working notes.
    pub context: TaskContext,
}

impl Task {
    /// Returns the artifact whose checkout the implementation should continue from.
    #[must_use]
    pub fn target(&self) -> Source {
        self.context
            .patch()
            .or_else(|| {
                self.context.artifacts.iter().find_map(|artifact| {
                    (artifact.role == ArtifactRole::Implementation)
                        .then(|| artifact.target().ok())
                        .flatten()
                        .filter(|subject| matches!(subject, Subject::Patch(_)))
                })
            })
            .map_or_else(|| self.source.clone(), Source::from)
    }

    pub(super) fn read(statement: &sqlite::Statement<'_>) -> sqlite::Result<Self> {
        let brief = statement.read::<String, _>("brief")?;
        let source = Source::from_str(&statement.read::<String, _>("source")?)
            .map_err(Self::stored_error)?;
        let context: TaskContext = serde_json::from_str(&statement.read::<String, _>("context")?)
            .map_err(Self::stored_error)?;
        context.validate().map_err(Self::stored_error)?;
        Ok(Self {
            id: statement.read::<i64, _>("id")?,
            brief,
            source,
            position: Self::position(statement.read::<i64, _>("position")?),
            context,
        })
    }

    const fn position(rank: i64) -> QueuePosition {
        match rank {
            0 => QueuePosition::Front,
            1 => QueuePosition::Next,
            _ => QueuePosition::End,
        }
    }

    pub(super) fn stored_error(error: impl std::fmt::Display) -> sqlite::Error {
        sqlite::Error {
            code: None,
            message: Some(error.to_string()),
        }
    }
}

/// A request to record work, with a stable identity for replay and assignment/mention reuse.
#[derive(Debug)]
pub struct TaskRequest {
    /// What was agreed.
    pub brief: String,
    /// Immutable place the work originated.
    pub source: Source,
    /// Requested queue position.
    pub position: QueuePosition,
    /// Stable request identity, absent for a deliberately fresh local task.
    pub request: Option<String>,
    /// Additional context supplied when the work is accepted.
    pub context: TaskContext,
    /// Explicitly separate work on the same issue; never repeats the same request identity.
    pub distinct: bool,
}

impl TaskRequest {
    /// Creates a fresh local request. Runtime deliveries also set its stable request identity.
    #[must_use]
    pub fn new(brief: String, source: Source, position: QueuePosition) -> Self {
        Self {
            context: TaskContext::new(brief.clone(), &source, None),
            brief,
            source,
            position,
            request: None,
            distinct: false,
        }
    }
}

/// State returned when recording or looking up a previously accepted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Work is queued.
    Waiting,
    /// Work is running.
    Running,
    /// The request has already been completed.
    Completed,
    /// The request was explicitly cancelled.
    Cancelled,
}

impl TaskState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(super) fn read(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Waiting,
        }
    }
}

/// Minimal outcome retained after a request's working context has been cleaned up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Receipt {
    /// Task that accepted this request.
    pub task: i64,
    /// Current or terminal state.
    pub state: TaskState,
    /// Published deliverable, if any.
    pub result: Option<String>,
}
