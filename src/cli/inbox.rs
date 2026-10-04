//! Read-only local or replicated delivery inspection.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use radicle::{
    identity::{Did, RepoId},
    storage::ReadStorage as _,
};

use crate::{
    delivery::{Record, remote::Deliveries},
    member::Member,
    status::Snapshot,
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
        let requested = match self {
            Self::Show { id } => Some(id.as_str()),
            Self::Ls => None,
        };
        let mut found = false;
        let mut lines = vec!["╭─ delivery inbox".to_owned()];
        for entry in entries
            .iter()
            .filter(|entry| requested.is_none_or(|id| entry.record.matches(id)))
        {
            found = true;
            lines.extend(entry.render(requested.is_some()));
        }
        if !found {
            lines.push(format!(
                "│  {}",
                if requested.is_some() {
                    "Not observed locally."
                } else {
                    "No observed deliveries."
                }
            ));
        }
        if replicated {
            lines.push(
                "│  Replicated observations; synchronize your node for newer receipts.".into(),
            );
        }
        lines.push("╰─ rad-team".into());
        lines.join("\n")
    }
}

struct Entry {
    member: Member,
    did: Option<Did>,
    record: Record,
}

impl Entry {
    fn render(&self, identity: bool) -> Vec<String> {
        let record = &self.record;
        let mut lines = vec![
            format!(
                "│  {} · {} · {}",
                self.member, record.state, record.activity.subject
            ),
            format!(
                "│    Operation: {} · action {}",
                record.activity.operation, record.activity.action
            ),
            format!("│    Author: {}", record.activity.author),
            format!("│    Event: {:?}", record.activity.kind),
        ];
        if let Some(task) = record.task {
            lines.push(format!("│    Task: #{task}"));
        }
        if identity && let Some(did) = self.did {
            lines.push(format!("│    Member identity: {did}"));
        }
        lines.push(format!(
            "│    Last observed: {}s ago",
            Snapshot::now().saturating_sub(record.updated)
        ));
        if let Some(error) = &record.error {
            lines.push(format!("│    Reason: {error}"));
        }
        lines.push("│".into());
        lines
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
