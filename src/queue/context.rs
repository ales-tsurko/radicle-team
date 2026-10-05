//! Specification and relationships retained while a task is unfinished.

use std::str::FromStr as _;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    agent::Source,
    queue::TaskKind,
    watcher::{Activity, Subject},
};

/// Durable working context shared by the member's talking and building roles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TaskContext {
    /// Runtime-owned work kind. A review assignment does not authorize implementation.
    pub kind: TaskKind,
    /// Latest repository request accepted by the runtime, independent of member-written notes.
    #[schemars(skip)]
    pub(crate) activity: Option<Activity>,
    /// Stable delivery or chat request that accepted the work.
    pub request: Option<String>,
    /// Current agreed specification, including acceptance criteria and exclusions.
    pub specification: String,
    /// Repository artifacts and their relationship to this task.
    pub artifacts: Vec<Artifact>,
    /// Messages worth rereading for specifications, decisions, and review feedback.
    pub messages: Vec<MessageReference>,
    /// Findings, blockers, and corrections retained across interruptions.
    pub notes: Vec<String>,
    /// Whether the runtime was explicitly asked to stop this task.
    pub cancelled: bool,
}

impl TaskContext {
    /// Retains a newly accepted correction when the runtime reuses existing work.
    pub(crate) fn accept(&mut self, context: Self, brief: &str) -> Result<(), crate::queue::Error> {
        if context.specification != brief {
            self.specification = context.specification;
        } else if self.specification != brief && !self.notes.iter().any(|note| note == brief) {
            self.notes.push(brief.to_owned());
        }
        if context.activity.is_some() {
            self.activity = context.activity;
        }
        for artifact in context.artifacts {
            self.relate(artifact);
        }
        self.update(ContextUpdate {
            messages: context.messages,
            notes: context.notes,
            ..ContextUpdate::default()
        })
    }

    /// Starts a context without changing the place the work originated.
    #[must_use]
    pub fn new(specification: String, source: &Source, request: Option<String>) -> Self {
        let artifacts = match source {
            Source::Subject(subject) => vec![Artifact {
                subject: subject.to_string(),
                role: ArtifactRole::Origin,
                purpose: "Original task".to_owned(),
            }],
            Source::Chat(_) => Vec::new(),
        };
        Self {
            specification,
            request,
            artifacts,
            ..Self::default()
        }
    }

    /// Returns the patch the runtime recorded as this task's deliverable.
    #[must_use]
    pub fn patch(&self) -> Option<Subject> {
        self.artifacts.iter().find_map(|artifact| {
            let subject = artifact.target().ok()?;
            (artifact.role == ArtifactRole::Produced && matches!(subject, Subject::Patch(_)))
                .then_some(subject)
        })
    }

    /// Records a relationship once, retaining its most specific purpose.
    pub fn relate(&mut self, artifact: Artifact) {
        if let Some(existing) = self
            .artifacts
            .iter_mut()
            .find(|existing| existing.subject == artifact.subject && existing.role == artifact.role)
        {
            *existing = artifact;
        } else {
            self.artifacts.push(artifact);
        }
    }

    /// Saves selected context without replacing the runtime's origin or produced artifacts.
    pub(crate) fn update(&mut self, update: ContextUpdate) -> Result<(), crate::queue::Error> {
        if update
            .artifacts
            .iter()
            .any(|artifact| matches!(artifact.role, ArtifactRole::Origin | ArtifactRole::Produced))
        {
            return Err(crate::queue::Error::InvalidContext(
                "origin and produced artifacts are recorded by the runtime".to_owned(),
            ));
        }
        if let Some(specification) = update.specification {
            self.specification = specification;
        }
        for artifact in update.artifacts {
            self.relate(artifact);
        }
        for message in update.messages {
            if let Some(existing) = self.messages.iter_mut().find(|existing| {
                existing.subject == message.subject
                    && existing.entry == message.entry
                    && existing.revision == message.revision
            }) {
                *existing = message;
            } else {
                self.messages.push(message);
            }
        }
        for note in update.notes {
            if !self.notes.contains(&note) {
                self.notes.push(note);
            }
        }
        self.validate()
    }

    /// Checks identifiers and keeps the context small enough to hand to a resumed agent.
    pub(crate) fn validate(&self) -> Result<(), crate::queue::Error> {
        for artifact in &self.artifacts {
            artifact.target()?;
        }
        for message in &self.messages {
            message.validate()?;
        }
        if serde_json::to_vec(self)?.len() > 64 * 1024 {
            return Err(crate::queue::Error::InvalidContext(
                "task context exceeds 64 KiB; retain references and concise notes".to_owned(),
            ));
        }
        Ok(())
    }
}

/// One repository artifact related to the task; a reference is not a permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    /// Typed reference, such as `issue:<id>` or `patch:<id>`.
    pub subject: String,
    /// Why the artifact belongs in the context.
    pub role: ArtifactRole,
    /// Concise explanation of the relationship.
    pub purpose: String,
}

impl Artifact {
    /// Parses the referenced artifact without permitting a repository change.
    pub(crate) fn target(&self) -> Result<Subject, crate::queue::Error> {
        Subject::from_str(&self.subject)
            .map_err(|error| crate::queue::Error::InvalidContext(error.to_string()))
    }
}

/// Purpose of an artifact in the task context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    /// Immutable origin recorded by the runtime.
    Origin,
    /// An existing artifact whose implementation is being changed.
    Implementation,
    /// An artifact being reviewed.
    Review,
    /// A published deliverable recorded by the runtime, including one this task revises.
    Produced,
    /// Supporting information; saving it does not authorize writes.
    Supporting,
}

/// A pinned message with the revision to which its evidence applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessageReference {
    /// Typed issue or patch reference containing the message.
    pub subject: String,
    /// Operation containing the specification, decision, or feedback.
    pub entry: String,
    /// Patch revision, when the message applies to one specific revision.
    pub revision: Option<String>,
    /// Why the message matters; the current source remains authoritative.
    pub purpose: String,
}

impl MessageReference {
    fn validate(&self) -> Result<(), crate::queue::Error> {
        Subject::from_str(&self.subject)
            .map_err(|error| crate::queue::Error::InvalidContext(error.to_string()))?;
        for entry in std::iter::once(&self.entry).chain(self.revision.iter()) {
            radicle::cob::EntryId::from_str(entry)
                .map_err(|error| crate::queue::Error::InvalidContext(error.to_string()))?;
        }
        Ok(())
    }
}

/// Incremental changes a member can save without replacing runtime-owned provenance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ContextUpdate {
    /// New agreed specification; absent leaves the current one intact.
    pub specification: Option<String>,
    /// Related artifacts to remember; origin and produced relationships are runtime-owned.
    pub artifacts: Vec<Artifact>,
    /// Specification and review messages to pin.
    pub messages: Vec<MessageReference>,
    /// Findings, blockers, and corrections to append.
    pub notes: Vec<String>,
}
