//! Human-readable member status and task context.

use std::{collections::BTreeSet, io::IsTerminal as _};

use console::Style;

use crate::{
    agent::Role,
    manifest::QueuePosition,
    queue::ArtifactRole,
    status::{Error, Inspection, Phase, Snapshot},
};

#[derive(Debug)]
pub(crate) struct View(pub(crate) Inspection);

impl View {
    pub(crate) fn status(&self) -> String {
        let inspection = &self.0;
        let style = Self::style();
        let dim = style.clone().dim();
        let mut lines = vec![String::new()];
        if let Some(failure) = inspection
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.failure.as_ref())
        {
            lines.push(
                style
                    .clone()
                    .red()
                    .apply_to(format!("Last error: {failure}"))
                    .to_string(),
            );
            lines.push(String::new());
        }
        for role in [Role::Coder, Role::Talker, Role::Chatter] {
            lines.extend(self.role(role));
        }
        lines.push(String::new());
        lines.push(
            style
                .clone()
                .bold()
                .apply_to(format!(
                    "Waiting tasks: {}",
                    inspection.tasks.iter().filter(|(_, taken)| !taken).count()
                ))
                .to_string(),
        );
        if let Some(snapshot) = &inspection.snapshot {
            lines.push(
                dim.apply_to(format!("Started: {}", Self::age(snapshot.started)))
                    .to_string(),
            );
            lines.push(
                dim.apply_to(format!("Last activity: {}", Self::age(snapshot.updated)))
                    .to_string(),
            );
        }
        lines.extend([
            String::new(),
            dim.apply_to(format!("Repository: {}", inspection.repository))
                .to_string(),
            dim.apply_to(format!("Home: {}", inspection.home.path().display()))
                .to_string(),
            String::new(),
        ]);
        let mut rendered = vec![format!(
            "{} {} · {}",
            dim.apply_to("╭─"),
            style.cyan().bold().apply_to(inspection.member),
            self.phase_badge(),
        )];
        rendered.extend(lines.iter().flat_map(|line| line.split('\n')).map(|line| {
            if line.is_empty() {
                dim.apply_to("│").to_string()
            } else {
                format!("{}  {line}", dim.apply_to("│"))
            }
        }));
        rendered.push(dim.apply_to("╰─ rad-team").to_string());
        rendered.join("\n")
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
            return vec![Self::role_header(role, "not started")];
        };
        if status.enabled == Some(false) {
            return vec![Self::role_header(role, "disabled")];
        }
        if let Some(activity) = &status.activity {
            let state = if self.0.live && self.0.phase().active() {
                "active"
            } else {
                "interrupted"
            };
            let mut lines = vec![Self::role_header(role, state)];
            if let Some(id) = activity.task {
                let brief = self
                    .0
                    .tasks
                    .iter()
                    .find(|(task, _)| task.id == id)
                    .map(|(task, _)| Self::brief(&task.brief))
                    .unwrap_or_default();
                lines.push(
                    Self::style()
                        .bold()
                        .apply_to(format!("  Task #{id}: {brief}"))
                        .to_string(),
                );
            }
            let dim = Self::style().dim();
            lines.push(
                dim.apply_to(format!("  Source: {}", activity.source))
                    .to_string(),
            );
            if let Some(started) = status.started {
                lines.push(
                    dim.apply_to(format!("  Started: {}", Self::age(started)))
                        .to_string(),
                );
            }
            if let Some(progress) = status.progress {
                lines.push(
                    dim.apply_to(format!("  Last progress: {}", Self::age(progress)))
                        .to_string(),
                );
            }
            lines.push(String::new());
            lines
        } else {
            let state = if status.enabled.is_none() {
                "not started"
            } else if self.0.live && self.0.phase().active() {
                "idle"
            } else {
                "stopped"
            };
            vec![Self::role_header(role, state)]
        }
    }

    fn phase_badge(&self) -> String {
        let style = Self::style();
        let phase = self.0.phase();
        let (symbol, color) = match phase {
            Phase::Running => ("●", style.green()),
            Phase::Failed => ("✗", style.red()),
            Phase::Interrupted => ("!", style.yellow()),
            Phase::Stopped => ("○", style.dim()),
            Phase::AwaitingApproval => ("◷", style.yellow()),
            _ => ("◷", style.cyan()),
        };
        color
            .bold()
            .apply_to(format!("{symbol} {phase}"))
            .to_string()
    }

    fn role_header(role: Role, state: &str) -> String {
        let style = Self::style();
        let (symbol, color) = match state {
            "active" => ("●", style.clone().green()),
            "interrupted" => ("!", style.clone().yellow()),
            "disabled" => ("─", style.clone().dim()),
            _ => ("○", style.clone().dim()),
        };
        format!(
            "{} {}: {}",
            color.apply_to(symbol),
            style.bold().apply_to(role),
            color.apply_to(state)
        )
    }

    fn style() -> Style {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Style::new().force_styling(
            std::io::stdout().is_terminal() && console::colors_enabled() && !no_color,
        )
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
