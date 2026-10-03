//! Human-readable member status and task context.

use std::collections::BTreeSet;

use crate::{
    agent::Role,
    manifest::QueuePosition,
    queue::ArtifactRole,
    status::{Error, Inspection, Snapshot},
};

#[derive(Debug)]
pub(crate) struct View(pub(crate) Inspection);

impl View {
    pub(crate) fn status(&self) -> String {
        let inspection = &self.0;
        let mut lines = vec![
            format!("Member: {}", inspection.member),
            format!("Home: {}", inspection.home.path().display()),
            format!("Repository: {}", inspection.repository),
            format!("Status: {}", inspection.phase()),
        ];
        if let Some(snapshot) = &inspection.snapshot {
            lines.push(format!("Started: {}", Self::age(snapshot.started)));
            lines.push(format!("Last activity: {}", Self::age(snapshot.updated)));
            if let Some(failure) = &snapshot.failure {
                lines.push(format!("Last error: {failure}"));
            }
        }
        for role in [Role::Coder, Role::Talker, Role::Chatter] {
            lines.extend(self.role(role));
        }
        lines.push(format!(
            "Waiting tasks: {}",
            inspection.tasks.iter().filter(|(_, taken)| !taken).count()
        ));
        lines.join("\n")
    }

    pub(crate) fn tasks(&self) -> String {
        if self.0.tasks.is_empty() {
            return "No unfinished tasks.".to_owned();
        }
        let mut lines = vec!["ID     STATE        PRIORITY  BRIEF".to_owned()];
        for (task, taken) in &self.0.tasks {
            lines.push(format!(
                "{:<6} {:<12} {:<9} {}",
                task.id,
                self.0.task_state(task, *taken),
                Self::priority(task.position),
                Self::brief(&task.brief)
            ));
            lines.push(format!("       {}", task.source));
            let source = task.source.to_string();
            for subject in task
                .context
                .artifacts
                .iter()
                .map(|artifact| &artifact.subject)
                .filter(|subject| **subject != source)
                .collect::<BTreeSet<_>>()
            {
                lines.push(format!("       {subject}"));
            }
        }
        lines.join("\n")
    }

    pub(crate) fn task(&self, id: i64) -> Result<String, Error> {
        let (task, taken) = self
            .0
            .tasks
            .iter()
            .find(|(task, _)| task.id == id)
            .ok_or(Error::MissingTask(id))?;
        let mut lines = vec![
            format!("Task {}: {}", task.id, self.0.task_state(task, *taken)),
            format!("Priority: {}", Self::priority(task.position)),
            format!("Source: {}", task.source),
        ];
        if task.context.cancelled {
            lines.push("Cancellation requested".to_owned());
        }
        lines.push(format!("\nSpecification:\n{}", task.context.specification));
        for artifact in &task.context.artifacts {
            let role = match artifact.role {
                ArtifactRole::Origin => "origin",
                ArtifactRole::Implementation => "implementation",
                ArtifactRole::Review => "review",
                ArtifactRole::Produced => "produced",
                ArtifactRole::Supporting => "supporting",
            };
            lines.push(format!(
                "\n{} ({role}): {}",
                artifact.subject, artifact.purpose
            ));
        }
        for message in &task.context.messages {
            lines.push(format!(
                "\n{} message {}: {}",
                message.subject, message.entry, message.purpose
            ));
            if let Some(revision) = &message.revision {
                lines.push(format!("Revision: {revision}"));
            }
        }
        if !task.context.notes.is_empty() {
            lines.push("\nNotes:".to_owned());
            lines.extend(task.context.notes.iter().cloned());
        }
        Ok(lines.join("\n"))
    }

    fn role(&self, role: Role) -> Vec<String> {
        let Some(status) = self
            .0
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.roles.get(&role))
        else {
            return vec![format!("{role}: not started")];
        };
        if status.enabled == Some(false) {
            return vec![format!("{role}: disabled")];
        }
        if let Some(activity) = &status.activity {
            let state = if self.0.live && self.0.phase().active() {
                "active"
            } else {
                "interrupted"
            };
            let mut lines = if let Some(id) = activity.task {
                let brief = self
                    .0
                    .tasks
                    .iter()
                    .find(|(task, _)| task.id == id)
                    .map(|(task, _)| Self::brief(&task.brief))
                    .unwrap_or_default();
                vec![format!("{role}: {state}, task {id}: {brief}")]
            } else {
                vec![format!("{role}: {state}")]
            };
            lines.push(format!("  Source: {}", activity.source));
            if let Some(started) = status.started {
                lines.push(format!("  Started: {}", Self::age(started)));
            }
            if let Some(progress) = status.progress {
                lines.push(format!("  Last progress: {}", Self::age(progress)));
            }
            lines
        } else {
            let state = if status.enabled.is_none() {
                "not started"
            } else if self.0.live && self.0.phase().active() {
                "idle"
            } else {
                "stopped"
            };
            vec![format!("{role}: {state}")]
        }
    }

    const fn priority(position: QueuePosition) -> &'static str {
        match position {
            QueuePosition::Front => "first",
            QueuePosition::Next => "next",
            QueuePosition::End => "normal",
        }
    }

    fn brief(brief: &str) -> String {
        let line = brief.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut short = line.chars().take(80).collect::<String>();
        if line.chars().count() > 80 {
            short.push('…');
        }
        short
    }

    fn age(at: u64) -> String {
        let seconds = Snapshot::now().saturating_sub(at);
        match seconds {
            0..60 => format!("{seconds}s ago"),
            60..3600 => format!("{}m {}s ago", seconds / 60, seconds % 60),
            _ => format!("{}h {}m ago", seconds / 3600, (seconds % 3600) / 60),
        }
    }
}
