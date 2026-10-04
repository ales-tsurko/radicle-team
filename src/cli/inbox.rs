//! Read-only local or replicated delivery inspection.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use console::Style;
use radicle::{
    identity::{Did, RepoId},
    storage::ReadStorage as _,
};

use crate::{
    cli::display::Display,
    delivery::{Record, State, remote::Deliveries},
    member::Member,
};

#[derive(Debug, Args)]
pub(super) struct Options {
    /// Member home; otherwise discover it from this directory.
    #[arg(long, global = true, conflicts_with = "repo")]
    pub(super) home: Option<PathBuf>,
    /// Inspect signed remote receipts already synchronized into your Radicle storage.
    #[arg(long, global = true)]
    repo: Option<RepoId>,
    /// Filter remote receipts by member.
    #[arg(long, global = true, requires = "repo")]
    member: Option<Member>,
    #[command(subcommand)]
    command: Command,
}

impl Options {
    pub(super) async fn execute(self) -> Result<(), Error> {
        let mut entries = self.read().await?;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.record.received));
        println!("{}", self.command.render(&entries, self.repo.is_some()));
        Ok(())
    }

    async fn read(&self) -> Result<Vec<Entry>, Error> {
        if let Some(repository) = self.repo {
            let member = self.member;
            Ok(tokio::task::spawn_blocking(move || {
                let profile = radicle::Profile::load()?;
                let repository = profile.storage.repository(repository)?;
                let mut records = Vec::new();
                for delivery in Deliveries::read(&repository)? {
                    if member.is_none_or(|member| member == delivery.member) {
                        records.extend(delivery.records.into_values().map(|record| Entry {
                            member: delivery.member,
                            did: Some(delivery.author.into()),
                            record,
                        }));
                    }
                }
                Ok::<_, Error>(records)
            })
            .await??)
        } else {
            let inspection = crate::status::Inspection::read(self.home.clone()).await?;
            Ok(inspection
                .deliveries
                .into_iter()
                .map(|record| Entry {
                    member: inspection.member,
                    did: inspection.did,
                    record,
                })
                .collect())
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List observed notifications and their delivery state.
    Ls,
    /// Find a review, comment, revision, subject, or operation ID.
    Show { id: String },
}

impl Command {
    fn render(&self, entries: &[Entry], replicated: bool) -> String {
        let style = Display::style();
        let dim = style.clone().dim();
        let requested = match self {
            Self::Show { id } => Some(id.as_str()),
            Self::Ls => None,
        };
        let mut found = false;
        let mut lines = vec![
            format!(
                "{} {}",
                dim.apply_to("╭─"),
                style.clone().bold().apply_to("delivery inbox")
            ),
            dim.apply_to("│").to_string(),
        ];
        for entry in entries
            .iter()
            .filter(|entry| requested.is_none_or(|id| entry.record.matches(id)))
        {
            found = true;
            lines.extend(entry.render(&style, requested.is_some()));
        }
        if !found {
            lines.push(
                dim.apply_to(format!(
                    "│  {}",
                    if requested.is_some() {
                        "Not observed locally."
                    } else {
                        "No observed deliveries."
                    }
                ))
                .to_string(),
            );
            lines.push(dim.apply_to("│").to_string());
        }
        if replicated {
            lines.push(
                dim.apply_to(
                    "│  Replicated observations; synchronize your node for newer receipts.",
                )
                .to_string(),
            );
        }
        lines.push(dim.apply_to("╰─ rad-team").to_string());
        lines.join("\n")
    }
}

struct Entry {
    member: Member,
    did: Option<Did>,
    record: Record,
}

impl Entry {
    fn render(&self, style: &Style, identity: bool) -> Vec<String> {
        let record = &self.record;
        let dim = style.clone().dim();
        let mut lines = vec![
            format!(
                "{}  {}  {}",
                dim.apply_to("│"),
                self.badge(style),
                style.clone().cyan().bold().apply_to(self.member),
            ),
            dim.apply_to(format!("│    {}", record.activity.subject))
                .to_string(),
            dim.apply_to(format!(
                "│    Operation: {} · action {}",
                record.activity.operation, record.activity.action
            ))
            .to_string(),
            dim.apply_to(format!("│    Author: {}", record.activity.author))
                .to_string(),
            dim.apply_to(format!("│    Event: {:?}", record.activity.kind))
                .to_string(),
        ];
        if let Some(task) = record.task {
            lines.push(format!(
                "{}    {}",
                dim.apply_to("│"),
                style.clone().bold().apply_to(format!("Task: #{task}")),
            ));
        }
        if identity && let Some(did) = self.did {
            lines.push(
                dim.apply_to(format!("│    Member identity: {did}"))
                    .to_string(),
            );
        }
        lines.push(
            dim.apply_to(format!(
                "│    Last observed: {}",
                Display::age(record.updated)
            ))
            .to_string(),
        );
        if let Some(error) = &record.error {
            let color = if record.state == State::Failed {
                style.clone().red()
            } else {
                style.clone().yellow()
            };
            lines.extend(error.lines().enumerate().map(|(index, line)| {
                format!(
                    "{}    {}",
                    dim.apply_to("│"),
                    color.apply_to(if index == 0 {
                        format!("Reason: {line}")
                    } else {
                        line.to_owned()
                    }),
                )
            }));
        }
        lines.push(dim.apply_to("│").to_string());
        lines
    }

    fn badge(&self, style: &Style) -> String {
        let (symbol, color) = match self.record.state {
            State::Queued => ("○", style.clone().cyan()),
            State::Processing => ("●", style.clone().green()),
            State::Handled => ("✓", style.clone().green()),
            State::Ignored => ("─", style.clone().dim()),
            State::Failed => ("!", style.clone().red()),
        };
        color
            .bold()
            .apply_to(format!("{symbol} {}", self.record.state))
            .to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Inspection(#[from] crate::status::Error),
    #[error(transparent)]
    Delivery(#[from] crate::delivery::Error),
    #[error(transparent)]
    Profile(#[from] radicle::profile::Error),
    #[error(transparent)]
    Repository(#[from] radicle::storage::RepositoryError),
    #[error(transparent)]
    Worker(#[from] tokio::task::JoinError),
}
