//! A consistent read of member activity and unfinished tasks without runtime side effects.

use std::{env, io, path::PathBuf};

use radicle::{
    crypto::ssh::Keystore,
    identity::{Did, RepoId},
};

use crate::{
    agent::Role,
    database::Database,
    home::MemberHome,
    identity::MemberLock,
    member::Member,
    queue::{Task, TaskQueue},
    status::{Error, Phase, Snapshot},
};

#[derive(Debug)]
pub(crate) struct Inspection {
    pub(crate) home: MemberHome,
    pub(crate) member: Member,
    pub(crate) repository: RepoId,
    pub(crate) did: Option<Did>,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) tasks: Vec<(Task, bool)>,
    pub(crate) deliveries: Vec<crate::delivery::Record>,
    pub(crate) live: bool,
}

impl Inspection {
    pub(crate) async fn read(path: Option<PathBuf>) -> Result<Self, Error> {
        let home = match path {
            Some(path) => MemberHome::at(path),
            None => MemberHome::find(
                &env::current_dir().map_err(crate::home::Error::WorkingDirectory)?,
            )?,
        };
        let marker = home.marker()?;
        let member = marker.member()?;
        let repository = marker
            .repository()?
            .ok_or_else(|| Error::UnboundIdentity(home.path().into()))?;
        let did = Keystore::new(&home.identity().join("keys"))
            .public_key()?
            .map(Did::from);
        let before = MemberLock::owner(&home)?;
        let (snapshot, tasks, deliveries) = Self::stored(&home, repository, member).await?;
        let after = MemberLock::owner(&home)?;
        let live = before == after
            && snapshot
                .as_ref()
                .is_some_and(|snapshot| after.as_deref() == Some(snapshot.session.as_str()));
        Ok(Self {
            home,
            member,
            repository,
            did,
            snapshot,
            tasks,
            deliveries,
            live,
        })
    }

    pub(crate) fn phase(&self) -> Phase {
        let phase = self
            .snapshot
            .as_ref()
            .map_or(Phase::Stopped, |snapshot| snapshot.phase);
        if phase.active() && !self.live {
            Phase::Interrupted
        } else {
            phase
        }
    }

    pub(crate) fn task_state(&self, task: &Task, taken: bool) -> &'static str {
        if !taken {
            return "waiting";
        }
        let running = self.live
            && self.phase() == Phase::Running
            && self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.roles.get(&Role::Coder))
                .and_then(|role| role.activity.as_ref())
                .is_some_and(|activity| activity.task == Some(task.id));
        if running { "running" } else { "interrupted" }
    }

    pub(crate) fn role_state(&self, role: Role) -> &'static str {
        let Some(status) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.roles.get(&role))
        else {
            return "not started";
        };
        if status.enabled == Some(false) {
            return "disabled";
        }
        if status.activity.is_some() {
            return if self.live && self.phase().active() {
                status.state()
            } else {
                "interrupted"
            };
        }
        if status.enabled.is_none() {
            "not started"
        } else if self.live && self.phase().active() {
            "idle"
        } else {
            "stopped"
        }
    }

    async fn stored(
        home: &MemberHome,
        repository: RepoId,
        member: Member,
    ) -> Result<
        (
            Option<Snapshot>,
            Vec<(Task, bool)>,
            Vec<crate::delivery::Record>,
        ),
        Error,
    > {
        match tokio::fs::metadata(home.state_database()).await {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((None, Vec::new(), Vec::new()));
            }
            Err(error) => return Err(error.into()),
        }
        let database = Database::read_only(home.state_database()).await?;
        let (data, tasks, deliveries) = database
            .call(move |connection| {
                connection.execute("BEGIN")?;
                let read = (|| {
                    let data = if Database::has_table(connection, "member_status")? {
                        let mut status =
                            connection.prepare("SELECT data FROM member_status WHERE id = 1")?;
                        if status.next()? == sqlite::State::Row {
                            Some(status.read::<String, _>(0)?)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    Ok((
                        data,
                        TaskQueue::inspect(connection, repository, member)?,
                        crate::delivery::Inbox::rows(connection)?,
                    ))
                })();
                connection.execute("ROLLBACK")?;
                read
            })
            .await?;
        Ok((
            data.map(|data| serde_json::from_str(&data)).transpose()?,
            tasks,
            deliveries
                .into_iter()
                .map(|data| serde_json::from_str(&data))
                .collect::<Result<_, _>>()?,
        ))
    }
}
