use std::{
    fs,
    time::{Duration, Instant},
};

use radicle::{
    crypto::{Seed, ssh::Keystore},
    identity::{Did, RepoId},
};

use crate::{
    agent::{Role, Session, Source},
    cli::inspection::View,
    database::Database,
    home::MemberHome,
    identity::MemberLock,
    manifest::QueuePosition,
    member::Member,
    queue::{Artifact, ArtifactRole, ContextUpdate, MessageReference, TaskQueue, TaskRequest},
    status::{Error, Inspection, Phase, Reporter},
};

struct Fixture {
    _directory: tempfile::TempDir,
    home: MemberHome,
    repository: RepoId,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("test directory should create");
        let home = MemberHome::at(directory.path());
        let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
            .parse()
            .expect("RID should parse");
        home.record(Member::Nico, repository)
            .expect("marker should write");
        Self {
            _directory: directory,
            home,
            repository,
        }
    }

    async fn read(&self) -> Inspection {
        Inspection::read(Some(self.home.path().into()))
            .await
            .expect("status should read")
    }

    async fn queue(&self) -> TaskQueue {
        TaskQueue::open(&self.home.state_database(), self.repository, Member::Nico)
            .await
            .expect("queue should open")
    }

    async fn push(queue: &TaskQueue, number: u32, position: QueuePosition) -> i64 {
        let source = Self::source(number);
        queue
            .push(TaskRequest::new(format!("Work {number}"), source, position))
            .await
            .expect("task should save")
            .task
    }

    fn source(number: u32) -> Source {
        format!("issue:{number:040x}")
            .parse()
            .expect("source should parse")
    }

    async fn reporter(&self, lock: &MemberLock) -> Reporter {
        Reporter::start(&self.home, lock.session())
            .await
            .expect("status should start")
    }
}

#[tokio::test]
async fn inspection_of_a_never_started_member_creates_nothing() {
    let fixture = Fixture::new();
    let marker =
        fs::read(fixture.home.path().join(crate::home::MARKER)).expect("marker should read");
    let view = View(fixture.read().await);
    let disabled = Reporter::default();
    disabled.phase(Phase::Running).await;
    disabled.ready(false).await;
    disabled.finish(None).await;
    assert_eq!(view.0.phase(), Phase::Stopped);
    assert!(view.status().contains("coder: not started"));
    assert!(view.tasks().contains("No unfinished tasks."));
    assert!(matches!(view.task(1), Err(Error::MissingTask(1))));
    assert!(!fixture.home.state_database().exists());
    assert!(!fixture.home.process_lock().exists());
    assert!(!fixture.home.identity().exists());
    assert!(!fixture.home.node_log().exists());
    assert_eq!(
        fs::read(fixture.home.path().join(crate::home::MARKER)).expect("marker should read"),
        marker
    );

    let keystore = Keystore::new(&fixture.home.identity().join("keys"));
    let key = keystore
        .init("nico", None, Seed::new(rand::random()))
        .expect("test identity should create");
    fs::remove_file(keystore.secret_key_path()).expect("secret key should remove");
    let public_path = keystore.public_key_path().expect("public key has a path");
    let public = fs::read(public_path).expect("public key should read");
    let view = View(fixture.read().await);
    assert_eq!(view.0.did, Some(Did::from(key)));
    assert!(view.status().contains(&format!("DID: {}", Did::from(key))));
    assert_eq!(
        fs::read(public_path).expect("public key should read"),
        public
    );
    assert!(!keystore.secret_key_path().exists());
    assert!(!fixture.home.state_database().exists());
    assert!(!fixture.home.process_lock().exists());
    assert!(!fixture.home.node_log().exists());
}

#[tokio::test]
async fn concurrent_roles_and_tasks_are_reported_without_mutating_the_queue() {
    let fixture = Fixture::new();
    let lock = MemberLock::acquire(&fixture.home).expect("member lock should acquire");
    let reporter = fixture.reporter(&lock).await;
    let queue = fixture.queue().await;
    let normal = Fixture::push(&queue, 1, QueuePosition::End).await;
    let next = Fixture::push(&queue, 2, QueuePosition::Next).await;
    let urgent = Fixture::push(&queue, 3, QueuePosition::Front).await;
    let last = Fixture::push(&queue, 4, QueuePosition::End).await;
    let taken = queue.next().await.expect("highest priority should take");
    assert_eq!(taken.id, urgent);
    reporter.ready(true).await;
    let coder = Session::new(Role::Coder);
    let talker = Session::new(Role::Talker);
    let chatter = Session::new(Role::Chatter);
    for session in [&coder, &talker, &chatter] {
        session.report_status(reporter.clone());
    }
    let subject = Fixture::source(9);
    let channel = Source::Chat("team".parse().expect("channel should parse"));
    tokio::join!(
        coder.handling(&taken.source, Some(taken.id)),
        talker.handling(&subject, None),
        chatter.handling(&channel, None)
    );

    let inspection = fixture.read().await;
    assert!(inspection.live);
    assert_eq!(inspection.phase(), Phase::Running);
    assert_eq!(
        inspection
            .tasks
            .iter()
            .map(|(task, _)| task.id)
            .collect::<Vec<_>>(),
        vec![urgent, next, normal, last]
    );
    let view = View(inspection);
    let status = view.status();
    for text in [
        "coder: active",
        &format!("Task #{urgent}: Work 3"),
        "talker: active",
        "chatter: active",
        "chat:team",
        "Waiting tasks: 3",
        "Last progress:",
    ] {
        assert!(status.contains(text), "{status}");
    }
    let tasks = view.tasks();
    for text in [
        "running",
        "waiting",
        "first",
        "next",
        "normal",
        &taken.source.to_string(),
    ] {
        assert!(tasks.contains(text), "{tasks}");
    }
    assert_eq!(
        queue
            .running()
            .await
            .expect("task should remain running")
            .id,
        urgent
    );
    assert_eq!(queue.waiting().await.expect("waiting count should read"), 3);

    queue
        .finish()
        .await
        .expect("completed context should remove");
    coder.handled().await;
    let view = View(fixture.read().await);
    assert!(view.status().contains("coder: idle"));
    assert!(view.status().contains("talker: active"));
    assert!(matches!(view.task(urgent), Err(Error::MissingTask(_))));
    reporter.finish(None).await;
    drop(lock);
    let stopped = View(fixture.read().await);
    assert_eq!(stopped.0.phase(), Phase::Stopped);
    assert!(stopped.status().contains("coder: stopped"));
    assert!(stopped.status().contains("talker: interrupted"));
}

#[tokio::test]
async fn crash_and_a_different_lock_owner_do_not_report_old_work_as_running() {
    let fixture = Fixture::new();
    let lock = MemberLock::acquire(&fixture.home).expect("lock should acquire");
    let reporter = fixture.reporter(&lock).await;
    let queue = fixture.queue().await;
    let id = Fixture::push(&queue, 1, QueuePosition::End).await;
    let task = queue.next().await.expect("task should take");
    reporter.ready(false).await;
    reporter
        .activity(
            Role::Coder,
            crate::status::Activity {
                task: Some(id),
                source: task.source.to_string(),
            },
        )
        .await;
    drop(lock);

    let crashed = View(fixture.read().await);
    assert_eq!(crashed.0.phase(), Phase::Interrupted);
    assert!(crashed.status().contains("coder: interrupted"));
    assert!(crashed.status().contains("chatter: disabled"));
    assert!(
        crashed
            .task(id)
            .expect("task should show")
            .contains("interrupted")
    );
    let other = MemberLock::acquire(&fixture.home).expect("another operation should lock");
    assert!(
        !fixture.read().await.live,
        "an unrelated setup must not revive old status"
    );
    let restarted = fixture.reporter(&other).await;
    let starting = fixture.read().await;
    assert_eq!(starting.phase(), Phase::Starting);
    assert_eq!(starting.task_state(&task, true), "interrupted");
    assert_eq!(queue.recover().await.expect("runtime should recover"), 1);
    restarted.ready(false).await;
    let view = View(fixture.read().await);
    assert!(view.tasks().contains("waiting"));
    assert!(view.status().contains("coder: idle"));
    restarted.finish(Some("provider unavailable".into())).await;
    assert_eq!(fixture.read().await.phase(), Phase::Failed);
    assert!(
        View(fixture.read().await)
            .status()
            .contains("Last error: provider unavailable")
    );
}

#[tokio::test]
async fn task_details_include_saved_specification_artifacts_messages_and_notes() {
    let fixture = Fixture::new();
    let queue = fixture.queue().await;
    let id = Fixture::push(&queue, 1, QueuePosition::End).await;
    let patch = format!("patch:{:040x}", 2);
    queue
        .update_context(
            id,
            ContextUpdate {
                specification: Some("Do the agreed work, preserving the accepted behavior.".into()),
                artifacts: [
                    ArtifactRole::Implementation,
                    ArtifactRole::Review,
                    ArtifactRole::Supporting,
                ]
                .into_iter()
                .map(|role| Artifact {
                    subject: patch.clone(),
                    role,
                    purpose: "Existing patch".into(),
                })
                .collect(),
                messages: vec![MessageReference {
                    subject: patch.clone(),
                    entry: format!("{:040x}", 3),
                    revision: Some(format!("{:040x}", 4)),
                    purpose: "Accepted specification".into(),
                }],
                notes: vec!["Waiting for the agreed decision".into()],
            },
        )
        .await
        .expect("context should update");
    queue
        .produced(id, patch.parse().expect("patch should parse"))
        .await
        .expect("produced patch should record");
    let taken = queue.next().await.expect("task should take");
    queue
        .cancel(taken.id)
        .await
        .expect("cancellation should request");
    let view = View(fixture.read().await);
    let detail = view.task(id).expect("unfinished task should show");
    for text in [
        "Specification:",
        "Do the agreed work",
        "origin",
        "implementation",
        "review",
        "supporting",
        "produced",
        "Accepted specification",
        "Revision:",
        "Waiting for the agreed decision",
        "Cancellation requested",
    ] {
        assert!(detail.contains(text), "{detail}");
    }
    assert!(view.tasks().contains(&patch));
    queue
        .finish()
        .await
        .expect("cancelled context should remove");
    assert!(matches!(
        View(fixture.read().await).task(id),
        Err(Error::MissingTask(_))
    ));
}

#[tokio::test]
async fn status_reports_startup_phases_and_observed_progress() {
    let fixture = Fixture::new();
    let lock = MemberLock::acquire(&fixture.home).expect("lock should acquire");
    let reporter = fixture.reporter(&lock).await;
    for phase in [
        Phase::Starting,
        Phase::Synchronizing,
        Phase::AwaitingApproval,
        Phase::Initializing,
        Phase::Stopping,
    ] {
        reporter.phase(phase).await;
        let view = View(fixture.read().await);
        assert_eq!(view.0.phase(), phase);
        assert!(view.status().contains(&phase.to_string()));
    }
    reporter.ready(false).await;
    let session = Session::new(Role::Coder);
    session.report_status(reporter);
    session.handling(&Fixture::source(1), Some(1)).await;
    let before = fixture
        .read()
        .await
        .snapshot
        .expect("snapshot should exist")
        .roles
        .get(&Role::Coder)
        .expect("coder should exist")
        .progress
        .expect("activity start should record");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let observed = Instant::now();
    session.checkpoint(observed).await;
    let after = fixture
        .read()
        .await
        .snapshot
        .expect("snapshot should exist")
        .roles
        .get(&Role::Coder)
        .expect("coder should exist")
        .progress
        .expect("observed progress should record");
    assert!(after > before);
    session.checkpoint(observed).await;
    session.handled().await;
    session.checkpoint(Instant::now()).await;
    assert!(View(fixture.read().await).status().contains("coder: idle"));
}

#[tokio::test]
async fn read_only_state_accepts_uninitialized_tables_and_refuses_writes_and_corrupt_data() {
    let fixture = Fixture::new();
    let database = Database::open(fixture.home.state_database())
        .await
        .expect("database should create");
    assert!(fixture.read().await.tasks.is_empty());
    let lock = MemberLock::acquire(&fixture.home).expect("lock should acquire");
    let _reporter = fixture.reporter(&lock).await;
    let read_only = Database::read_only(fixture.home.state_database())
        .await
        .expect("database should read");
    read_only
        .execute("DELETE FROM member_status")
        .await
        .expect_err("inspection must never write");
    assert!(fixture.read().await.snapshot.is_some());
    database
        .execute("DELETE FROM member_status")
        .await
        .expect("row should remove");
    assert!(fixture.read().await.snapshot.is_none());
    database
        .execute("INSERT INTO member_status VALUES (1, 'corrupt', 0, 'invalid')")
        .await
        .expect("corrupt snapshot should insert");
    assert!(matches!(
        Inspection::read(Some(fixture.home.path().into())).await,
        Err(Error::Snapshot(_))
    ));
}
