//! Human-readable member status and task context.

use std::collections::BTreeSet;

use crate::{
    agent::Role,
    cli::display::Display,
    manifest::QueuePosition,
    queue::ArtifactRole,
    status::{Error, Inspection, Phase},
};

#[derive(Debug)]
pub(crate) struct View(pub(crate) Inspection);

impl View {
    pub(crate) fn status(&self) -> String {
        let inspection = &self.0;
        let style = Display::style();
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
        lines.push(format!(
            "Queued notifications: {}",
            inspection
                .deliveries
                .iter()
                .filter(|record| record.task.is_none()
                    && record.state == crate::delivery::State::Queued)
                .count()
        ));
        for record in inspection.deliveries.iter().filter(|record| {
            record.task.is_none()
                && matches!(
                    record.state,
                    crate::delivery::State::Processing | crate::delivery::State::Failed
                )
        }) {
            lines.push(format!(
                "  {}: {} · {}",
                if inspection.live {
                    record.state.to_string()
                } else {
                    "interrupted".into()
                },
                record.activity.subject,
                record.activity.operation
            ));
            if let Some(error) = &record.error {
                lines.push(format!("  {error}"));
            }
        }
        if let Some(snapshot) = &inspection.snapshot {
            if let Some(fetched) = snapshot.fetched {
                lines.push(format!("Last successful fetch: {}", Display::age(fetched)));
            }
            if let Some(error) = &snapshot.sync_error {
                lines.push(format!("Sync error: {error}"));
            }
            lines.push(
                dim.apply_to(format!("Started: {}", Display::age(snapshot.started)))
                    .to_string(),
            );
            lines.push(
                dim.apply_to(format!("Last activity: {}", Display::age(snapshot.updated)))
                    .to_string(),
            );
        }
        lines.push(String::new());
        if let Some(did) = inspection.did {
            lines.push(dim.apply_to(format!("DID: {did}")).to_string());
        }
        lines.extend([
            dim.apply_to(format!("Repository: {}", inspection.repository))
                .to_string(),
            dim.apply_to(format!("Home: {}", inspection.home.path().display()))
                .to_string(),
            String::new(),
        ]);
        self.panel(&self.phase_badge(), lines)
    }

    pub(crate) fn tasks(&self) -> String {
        let style = Display::style();
        let dim = style.clone().dim();
        let mut lines = vec![String::new()];
        if self.0.tasks.is_empty() {
            lines.push(dim.apply_to("No unfinished tasks.").to_string());
            lines.push(String::new());
            return self.panel("task queue", lines);
        }
        for (task, taken) in &self.0.tasks {
            lines.push(format!(
                "{}  {} · {}",
                Self::task_badge(self.0.task_state(task, *taken)),
                style.clone().bold().apply_to(format!(
                    "#{}: {}",
                    task.id,
                    Self::brief(&task.brief)
                )),
                dim.apply_to(format!("priority {}", Self::priority(task.position))),
            ));
            lines.push(dim.apply_to(format!("  {}", task.source)).to_string());
            let source = task.source.to_string();
            for subject in task
                .context
                .artifacts
                .iter()
                .map(|artifact| &artifact.subject)
                .filter(|subject| **subject != source)
                .collect::<BTreeSet<_>>()
            {
                lines.push(dim.apply_to(format!("  {subject}")).to_string());
            }
            lines.push(String::new());
        }
        self.panel("task queue", lines)
    }

    pub(crate) fn task(&self, id: i64) -> Result<String, Error> {
        let (task, taken) = self
            .0
            .tasks
            .iter()
            .find(|(task, _)| task.id == id)
            .ok_or(Error::MissingTask(id))?;
        let style = Display::style();
        let dim = style.clone().dim();
        let mut lines = vec![
            String::new(),
            style.clone().bold().apply_to(&task.brief).to_string(),
            Self::task_badge(self.0.task_state(task, *taken)),
            dim.apply_to(format!("Priority: {}", Self::priority(task.position)))
                .to_string(),
            dim.apply_to(format!("Source: {}", task.source)).to_string(),
        ];
        if task.context.cancelled {
            lines.push(
                style
                    .yellow()
                    .apply_to("Cancellation requested")
                    .to_string(),
            );
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
        lines.push(String::new());
        Ok(self.panel(&format!("task #{}", task.id), lines))
    }

    fn panel(&self, heading: &str, lines: Vec<String>) -> String {
        let style = Display::style();
        let dim = style.clone().dim();
        let mut rendered = vec![format!(
            "{} {} · {heading}",
            dim.apply_to("╭─"),
            style.cyan().bold().apply_to(self.0.member),
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

    fn task_badge(state: &str) -> String {
        let style = Display::style();
        let (symbol, color) = match state {
            "running" => ("●", style.green()),
            "interrupted" => ("!", style.yellow()),
            _ => ("○", style.cyan()),
        };
        color.apply_to(format!("{symbol} {state}")).to_string()
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
                    Display::style()
                        .bold()
                        .apply_to(format!("  Task #{id}: {brief}"))
                        .to_string(),
                );
            }
            let dim = Display::style().dim();
            lines.push(
                dim.apply_to(format!("  Source: {}", activity.source))
                    .to_string(),
            );
            if let Some(started) = status.started {
                lines.push(
                    dim.apply_to(format!("  Started: {}", Display::age(started)))
                        .to_string(),
                );
            }
            if let Some(progress) = status.progress {
                lines.push(
                    dim.apply_to(format!("  Last progress: {}", Display::age(progress)))
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
        let style = Display::style();
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
        let style = Display::style();
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
}
