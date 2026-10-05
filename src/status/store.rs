//! Writes made by the runtime, serialized across its concurrent roles.

use std::{sync::Arc, time::Instant};

use tokio::sync::Mutex;

use crate::{
    agent::Role,
    database::Database,
    home::MemberHome,
    status::{Activity, Error, Observation, Phase, Snapshot},
};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS member_status (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    session TEXT NOT NULL, revision INTEGER NOT NULL, data TEXT NOT NULL
) STRICT;";

/// An optional observer. Setup and publication can use a disabled reporter without status writes.
#[derive(Clone, Default)]
pub(crate) struct Reporter(Option<Arc<Store>>);

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter").finish_non_exhaustive()
    }
}

impl Reporter {
    pub(crate) async fn start(home: &MemberHome, session: &str) -> Result<Self, Error> {
        let database = Database::open(home.state_database()).await?;
        database.execute(SCHEMA).await?;
        let snapshot = Snapshot::new(session.to_owned());
        let store = Store {
            database,
            snapshot: Mutex::new(snapshot),
        };
        store.save(&*store.snapshot.lock().await).await?;
        Ok(Self(Some(Arc::new(store))))
    }

    pub(crate) async fn phase(&self, phase: Phase) {
        self.change(move |snapshot| {
            snapshot.phase = phase;
            snapshot.updated = Snapshot::now();
            true
        })
        .await;
    }

    pub(crate) async fn ready(&self, chat: bool) {
        self.change(move |snapshot| {
            snapshot.phase = Phase::Running;
            snapshot.updated = Snapshot::now();
            for (role, status) in &mut snapshot.roles {
                status.enabled = Some(*role != Role::Chatter || chat);
            }
            true
        })
        .await;
    }

    pub(crate) async fn activity(&self, role: Role, activity: Activity) {
        self.change(move |snapshot| {
            let now = Snapshot::now();
            if let Some(status) = snapshot.roles.get_mut(&role) {
                status.activity = Some(activity);
                status.started = Some(now);
                status.progress = Some(now);
                status.observation = None;
            }
            snapshot.updated = now;
            true
        })
        .await;
    }

    pub(crate) async fn idle(&self, role: Role) {
        self.change(move |snapshot| {
            if let Some(status) = snapshot.roles.get_mut(&role) {
                status.activity = None;
                status.started = None;
            }
            snapshot.updated = Snapshot::now();
            true
        })
        .await;
    }

    pub(crate) async fn progress(&self, role: Role, at: Instant, observation: Option<Observation>) {
        let observed = Snapshot::observed(at);
        self.change(move |snapshot| {
            let Some(status) = snapshot.roles.get_mut(&role) else {
                return false;
            };
            if status.activity.is_none()
                || (status.progress.is_some_and(|last| last >= observed)
                    && status.observation == observation)
            {
                return false;
            }
            status.progress = Some(status.progress.unwrap_or_default().max(observed));
            status.observation = observation;
            snapshot.updated = snapshot.updated.max(observed);
            true
        })
        .await;
    }

    pub(crate) async fn retrying(&self, role: Role, detail: String) {
        self.change(move |snapshot| {
            if let Some(status) = snapshot.roles.get_mut(&role) {
                status.observation = Some(Observation {
                    state: crate::status::AgentState::Retrying,
                    at: Snapshot::now(),
                    tool: None,
                    completed: status
                        .observation
                        .as_ref()
                        .and_then(|observation| observation.completed),
                    detail: Some(detail),
                });
            }
            snapshot.updated = Snapshot::now();
            true
        })
        .await;
    }

    pub(crate) async fn synchronized(&self, error: Option<String>) {
        self.change(move |snapshot| {
            if error.is_none() {
                snapshot.fetched = Some(Snapshot::now());
            }
            snapshot.sync_error = error;
            snapshot.updated = Snapshot::now();
            true
        })
        .await;
    }

    pub(crate) async fn finish(&self, failure: Option<String>) {
        self.change(move |snapshot| {
            snapshot.phase = if failure.is_some() {
                Phase::Failed
            } else {
                Phase::Stopped
            };
            snapshot.failure = failure;
            snapshot.updated = Snapshot::now();
            true
        })
        .await;
    }

    // Reporting failure must not turn a successful publication into a failed delivery and retry it.
    async fn change(&self, update: impl FnOnce(&mut Snapshot) -> bool) {
        let Some(store) = &self.0 else {
            return;
        };
        let mut snapshot = store.snapshot.lock().await;
        if update(&mut snapshot) {
            snapshot.revision = snapshot.revision.saturating_add(1);
            if let Err(error) = store.save(&snapshot).await {
                tracing::warn!(%error, "could not record member status");
            }
        }
    }
}

struct Store {
    database: Database,
    snapshot: Mutex<Snapshot>,
}

impl Store {
    async fn save(&self, snapshot: &Snapshot) -> Result<(), Error> {
        let data = serde_json::to_string(snapshot)?;
        let session = snapshot.session.clone();
        let revision = snapshot.revision;
        self.database
            .call(move |connection| {
                let mut save = connection.prepare(
                    "INSERT INTO member_status (id, session, revision, data) VALUES (1, ?1, ?2, \
                     ?3)
                     ON CONFLICT (id) DO UPDATE SET session = excluded.session,
                     revision = excluded.revision, data = excluded.data
                     WHERE excluded.revision = 0 OR (member_status.session = excluded.session
                     AND member_status.revision < excluded.revision)",
                )?;
                save.bind((1, session.as_str()))?;
                save.bind((2, revision))?;
                save.bind((3, data.as_str()))?;
                save.next()?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        home::MemberHome,
        identity::MemberLock,
        member::Member,
        status::{Inspection, Phase, Reporter},
    };

    // A cancelled async write may still run on the blocking pool. It must not overwrite a later
    // snapshot or one belonging to a new lock acquisition.
    #[tokio::test]
    async fn delayed_writes_cannot_overwrite_newer_status_or_a_new_runtime() {
        let directory = tempfile::tempdir().expect("test directory should create");
        let home = MemberHome::at(directory.path());
        home.record(
            Member::Nico,
            "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
                .parse()
                .expect("RID should parse"),
        )
        .expect("marker should write");
        let lock = MemberLock::acquire(&home).expect("lock should acquire");
        let reporter = Reporter::start(&home, lock.session())
            .await
            .expect("status should start");
        reporter.phase(Phase::Synchronizing).await;
        let store = reporter.0.as_ref().expect("runtime should report status");
        let delayed = store.snapshot.lock().await.clone();
        reporter.ready(false).await;
        store
            .save(&delayed)
            .await
            .expect("delayed write should be ignored");
        let read = Inspection::read(Some(home.path().into()))
            .await
            .expect("status should read");
        assert_eq!(read.phase(), Phase::Running);
        drop(lock);
        let next = MemberLock::acquire(&home).expect("new runtime should acquire");
        let _next_reporter = Reporter::start(&home, next.session())
            .await
            .expect("new status should start");
        reporter.finish(Some("old failure".into())).await;
        let read = Inspection::read(Some(home.path().into()))
            .await
            .expect("new status should read");
        assert_eq!(read.phase(), Phase::Starting);
        assert_eq!(
            read.snapshot.expect("snapshot should exist").session,
            next.session()
        );
    }
}
