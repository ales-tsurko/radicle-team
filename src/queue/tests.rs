use std::{str::FromStr as _, time::Duration};

use radicle::{cob::ObjectId, identity::RepoId};

use crate::{
    agent::Source, database::Database, manifest::QueuePosition, member::Member, queue::TaskQueue,
    watcher::Subject,
};

const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const SUBJECT: &str = "1111111111111111111111111111111111111111";
const PRODUCED: &str = "2222222222222222222222222222222222222222";

async fn queue() -> TaskQueue {
    let database = Database::memory().await.expect("database should open");
    database
        .execute(crate::queue::store::SCHEMA)
        .await
        .expect("schema should apply");
    TaskQueue::with_database(
        database,
        RepoId::from_str(REPOSITORY).expect("repository should parse"),
        Member::Nico,
    )
}

fn subject() -> Source {
    Subject::Issue(ObjectId::from_str(SUBJECT).expect("subject should parse")).into()
}

/// A task which produced a patch and then failed must come back as a task about that patch.
/// Coming back as the issue it started on would begin the work again and orphan the patch.
#[tokio::test]
async fn remembers_the_patch_a_task_produced_across_a_restart() {
    let queue = queue().await;
    let patch = Subject::Patch(ObjectId::from_str(PRODUCED).expect("patch should parse"));
    let id = queue
        .push(crate::queue::TaskRequest::new(
            "answer it".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;
    let taken = queue.next().await.expect("queue should be readable");
    assert_eq!(taken.source, subject());

    queue
        .produced(id, patch)
        .await
        .expect("the task should be retargeted");
    assert_eq!(
        queue.running().await.map(|running| running.target()),
        Some(patch.into()),
        "the running task must be about the patch from here on"
    );

    assert_eq!(
        queue.recover().await.expect("queue should be readable"),
        1,
        "a task which never finished must come back"
    );
    let recovered = queue.next().await.expect("queue should be readable");

    assert_eq!(recovered.source, subject());
    assert_eq!(recovered.context.patch(), Some(patch));
}

#[tokio::test]
async fn runs_urgent_work_before_older_waiting_work() {
    let queue = queue().await;
    for (brief, position) in [
        ("first", QueuePosition::End),
        ("second", QueuePosition::End),
        ("blocking somebody", QueuePosition::Next),
        ("broken now", QueuePosition::Front),
    ] {
        let _id = queue
            .push(crate::queue::TaskRequest::new(
                brief.to_owned(),
                subject(),
                position,
            ))
            .await
            .expect("task should be recorded")
            .task;
    }

    let mut order = Vec::new();
    while let Some(task) = queue.take().await.expect("queue should be readable") {
        order.push(task.brief);
    }
    assert_eq!(
        order,
        ["broken now", "blocking somebody", "first", "second"],
        "position must decide first and arrival second, so that work nobody marked urgent still \
         runs in the order it was promised"
    );
}

#[tokio::test]
async fn keeps_the_brief_and_its_source_together() {
    let queue = queue().await;
    let _id = queue
        .push(crate::queue::TaskRequest::new(
            "Use Postgres, not MySQL".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;

    let pending = queue.pending().await.expect("queue should be readable");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].brief, "Use Postgres, not MySQL");
    assert_eq!(
        pending[0].source,
        subject(),
        "a task is only traceable back to its discussion through its source"
    );

    let taken = queue.take().await.expect("queue should be readable");
    assert!(taken.is_some(), "the recorded task should be takeable");
    assert!(
        queue
            .pending()
            .await
            .expect("queue should be readable")
            .is_empty(),
        "a taken task must not be handed out twice"
    );
}

#[tokio::test]
async fn puts_work_interrupted_by_a_dead_run_back_at_the_front() {
    let queue = queue().await;
    queue
        .push(crate::queue::TaskRequest::new(
            "first".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("first task should be recorded");
    queue
        .push(crate::queue::TaskRequest::new(
            "second".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("second task should be recorded");
    let taken = queue
        .take()
        .await
        .expect("queue should be readable")
        .expect("a task should be takeable");
    assert_eq!(taken.brief, "first");

    // The run dies here, leaving the task marked taken and nothing to finish it.
    assert_eq!(
        queue.recover().await.expect("queue should recover"),
        1,
        "a task left marked taken belonged to a run which died"
    );
    assert_eq!(
        queue
            .take()
            .await
            .expect("queue should be readable")
            .expect("the recovered task should be takeable")
            .brief,
        "first",
        "recovered work must run before anything queued behind it"
    );
}

#[tokio::test]
async fn forgets_work_which_actually_finished() {
    let queue = queue().await;
    queue
        .push(crate::queue::TaskRequest::new(
            "only".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded");
    let _taken = queue.next().await.expect("task should be taken");
    queue.finish().await.expect("task should be removed");

    assert_eq!(
        queue.recover().await.expect("queue should recover"),
        0,
        "a finished task must not come back as interrupted work"
    );
    assert_eq!(
        queue.waiting().await.expect("queue should be readable"),
        0,
        "a finished task must not stay in the queue"
    );
}

#[tokio::test]
async fn waits_for_work_that_has_not_arrived_yet() {
    let queue = queue().await;
    let waiting = tokio::spawn({
        let queue = queue.clone();
        async move { queue.next().await }
    });

    let _id = queue
        .push(crate::queue::TaskRequest::new(
            "write it".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;

    let task = waiting
        .await
        .expect("waiting task should finish")
        .expect("queue should be readable");
    assert_eq!(task.brief, "write it");
    assert_eq!(
        queue.running().await.map(|running| running.id),
        Some(task.id),
        "a task handed out must be reportable as running while it lasts"
    );

    queue.finish().await.expect("task should be removed");
    assert!(queue.running().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_request_to_stop_never_outlives_the_task_it_named() {
    let queue = queue().await;
    for brief in ["first", "second"] {
        let _id = queue
            .push(crate::queue::TaskRequest::new(
                brief.to_owned(),
                subject(),
                QueuePosition::End,
            ))
            .await
            .expect("task should be recorded")
            .task;
    }
    let first = queue.next().await.expect("queue should be readable");

    assert_eq!(
        queue
            .cancel(first.id)
            .await
            .expect("cancellation should save")
            .id,
        first.id,
        "a request to stop reports the task it reached"
    );
    queue.finish().await.expect("task should be removed");
    let second = queue.next().await.expect("queue should be readable");

    assert!(
        tokio::time::timeout(Duration::from_secs(1), queue.cancelled(second.id))
            .await
            .is_err(),
        "a request aimed at a task that has already ended must not stop the next one"
    );
    queue
        .cancel(second.id)
        .await
        .expect("cancellation should save");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), queue.cancelled(second.id))
            .await
            .is_ok(),
        "a request aimed at the running task must reach it"
    );
}

#[tokio::test]
async fn hands_every_note_to_the_running_task_once() {
    let queue = queue().await;
    let _id = queue
        .push(crate::queue::TaskRequest::new(
            "write it".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;
    let task = queue.next().await.expect("queue should be readable");

    for note in ["use the existing parser", "skip the migration"] {
        assert_eq!(
            queue
                .note(task.id, note.to_owned())
                .await
                .expect("note should save")
                .id,
            task.id
        );
    }
    assert_eq!(
        queue.notes(task.id).await,
        ["use the existing parser", "skip the migration"]
    );
    assert!(
        queue.notes(task.id).await.is_empty(),
        "a note already read must not repeat on the next tool call"
    );
}

#[tokio::test]
async fn refuses_a_note_with_nothing_running() {
    let queue = queue().await;

    assert!(
        queue.note(999, "too late".to_owned()).await.is_err(),
        "a note nobody is there to read must be refused, not left for the next task"
    );
    assert!(queue.notes(999).await.is_empty());
}

#[tokio::test]
async fn retains_specification_messages_notes_and_origin_across_restarts() {
    use crate::queue::{ContextUpdate, MessageReference};

    let root = tempfile::tempdir().expect("temporary home should open");
    let path = root.path().join("state.db");
    let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
    let original = TaskQueue::open(&path, repository, Member::Nico)
        .await
        .expect("queue should open");
    let mut request =
        crate::queue::TaskRequest::new("scaffold only".to_owned(), subject(), QueuePosition::End);
    request.request = Some("operation:specification".to_owned());
    let id = original
        .push(request)
        .await
        .expect("request should persist")
        .task;
    original
        .update_context(
            id,
            ContextUpdate {
                specification: Some("Correct the base; preserve the reviewed head".to_owned()),
                messages: vec![MessageReference {
                    subject: subject().to_string(),
                    entry: PRODUCED.to_owned(),
                    revision: None,
                    purpose: "Base-only review request".to_owned(),
                }],
                ..ContextUpdate::default()
            },
        )
        .await
        .expect("specification should persist");
    original
        .note(id, "Do not rewrite the scaffold".to_owned())
        .await
        .expect("waiting correction should persist");
    original.next().await.expect("task should start");
    let patch = Subject::Patch(ObjectId::from_str(PRODUCED).expect("patch should parse"));
    original
        .produced(id, patch)
        .await
        .expect("patch should persist");
    drop(original);

    let restarted = TaskQueue::open(&path, repository, Member::Nico)
        .await
        .expect("queue should reopen");
    restarted
        .recover()
        .await
        .expect("interrupted task should recover");
    let mut replay =
        crate::queue::TaskRequest::new("scaffold only".to_owned(), subject(), QueuePosition::End);
    replay.request = Some("operation:specification".to_owned());
    let receipt = restarted
        .push(replay)
        .await
        .expect("request should reuse recovered task");
    assert_eq!(receipt.task, id);
    assert_eq!(receipt.state, crate::queue::TaskState::Waiting);
    let task = restarted.next().await.expect("task should resume");
    assert_eq!(task.source, subject());
    assert_eq!(task.target(), patch.into());
    assert_eq!(
        task.context.specification,
        "Correct the base; preserve the reviewed head"
    );
    assert_eq!(task.context.messages.len(), 1);
    assert_eq!(task.context.notes, ["Do not rewrite the scaffold"]);
}

#[tokio::test]
async fn reuses_requests_and_cleans_context_while_retaining_completion() {
    let queue = queue().await;
    let request = || {
        let mut request =
            crate::queue::TaskRequest::new("scaffold".to_owned(), subject(), QueuePosition::End);
        request.request = Some("operation:one".to_owned());
        request
    };
    let first = queue.push(request()).await.expect("request should record");
    assert_eq!(
        queue
            .push(request())
            .await
            .expect("request should reuse")
            .task,
        first.task
    );
    queue.next().await.expect("task should start");
    let patch = Subject::Patch(ObjectId::from_str(PRODUCED).expect("patch should parse"));
    queue
        .produced(first.task, patch)
        .await
        .expect("patch should record");
    queue.finish().await.expect("task should finish");
    queue
        .get(first.task)
        .await
        .expect_err("completed working context should be removed");
    let contexts = queue
        .database()
        .call(|connection| {
            let mut select = connection.prepare("SELECT COUNT(*) FROM task_contexts")?;
            select.next()?;
            select.read::<i64, _>(0)
        })
        .await
        .expect("context count should read");
    assert_eq!(contexts, 0);
    let completed = queue
        .push(request())
        .await
        .expect("completed request should reuse");
    assert_eq!(completed.state, crate::queue::TaskState::Completed);
    assert_eq!(completed.result, Some(patch.to_string()));
    assert_eq!(queue.waiting().await.expect("queue should read"), 0);

    let mut followup = request();
    followup.request = Some("operation:correction".to_owned());
    let correction = queue
        .push(followup)
        .await
        .expect("new feedback should record work");
    assert_ne!(correction.task, first.task);
    let revision = queue.next().await.expect("revision should start");
    assert_eq!(revision.source, subject());
    assert_eq!(revision.context.patch(), Some(patch));
    assert_eq!(revision.target(), patch.into());
}

#[tokio::test]
async fn joins_related_work_but_allows_an_explicit_distinct_task() {
    let queue = queue().await;
    let request = |key: &str, distinct| {
        let mut request =
            crate::queue::TaskRequest::new("scaffold".to_owned(), subject(), QueuePosition::End);
        request.request = Some(key.to_owned());
        request.distinct = distinct;
        request
    };
    let assigned = queue
        .push(request("assignment", false))
        .await
        .expect("assignment should record");
    let mention = queue
        .push(request("mention", false))
        .await
        .expect("mention should reuse");
    assert_eq!(assigned.task, mention.task);
    let separate = queue
        .push(request("separate deliverable", true))
        .await
        .expect("distinct task should record");
    assert_ne!(assigned.task, separate.task);
    assert_eq!(
        queue
            .push(request("separate deliverable", true))
            .await
            .expect("distinct replay should reuse")
            .task,
        separate.task
    );
}

#[tokio::test]
async fn unrelated_chat_requests_do_not_share_tasks_or_completed_deliverables() {
    let queue = queue().await;
    let request = |key: &str| {
        let mut request = crate::queue::TaskRequest::new(
            key.to_owned(),
            Source::from_str("chat:general").expect("channel should parse"),
            QueuePosition::End,
        );
        request.request = Some(key.to_owned());
        request
    };
    let first = queue
        .push(request("chat:1"))
        .await
        .expect("first task should record");
    let second = queue
        .push(request("chat:2"))
        .await
        .expect("second task should record");
    assert_ne!(first.task, second.task);
    assert_eq!(
        queue
            .push(request("chat:1"))
            .await
            .expect("replay should reuse")
            .task,
        first.task
    );
    queue.next().await.expect("first task should start");
    let patch = Subject::Patch(ObjectId::from_str(PRODUCED).expect("patch should parse"));
    queue
        .produced(first.task, patch)
        .await
        .expect("deliverable should record");
    queue.finish().await.expect("first task should finish");
    let third = queue
        .push(request("chat:3"))
        .await
        .expect("another task should record");
    assert_ne!(second.task, third.task);
    assert!(
        queue
            .get(third.task)
            .await
            .expect("third task should read")
            .context
            .patch()
            .is_none()
    );
}

#[tokio::test]
async fn corrects_and_cancels_waiting_work_without_stopping_another_task() {
    let queue = queue().await;
    let first = queue
        .push(crate::queue::TaskRequest::new(
            "first".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("request should record")
        .task;
    let second = queue
        .push(crate::queue::TaskRequest::new(
            "second".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("request should record")
        .task;
    queue.next().await.expect("first task should start");
    queue
        .note(second, "explanation only".to_owned())
        .await
        .expect("waiting note should save");
    assert!(queue.notes(first).await.is_empty());
    assert_eq!(
        queue
            .get(second)
            .await
            .expect("second should read")
            .context
            .notes,
        ["explanation only"]
    );
    queue.cancel(second).await.expect("second should cancel");
    assert_eq!(
        queue.running().await.expect("first remains running").id,
        first
    );
    queue
        .get(second)
        .await
        .expect_err("cancelled waiting work should be removed");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), queue.cancelled(first))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn members_cannot_change_another_members_task_or_forge_produced_artifacts() {
    use crate::queue::{Artifact, ArtifactRole, ContextUpdate};

    let root = tempfile::tempdir().expect("temporary directory should open");
    let path = root.path().join("state.db");
    let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
    let nico = TaskQueue::open(&path, repository, Member::Nico)
        .await
        .expect("queue should open");
    let maya = TaskQueue::open(&path, repository, Member::Maya)
        .await
        .expect("queue should open");
    let id = nico
        .push(crate::queue::TaskRequest::new(
            "scaffold".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    maya.get(id)
        .await
        .expect_err("Maya cannot read Nico's task");
    maya.cancel(id)
        .await
        .expect_err("Maya cannot cancel Nico's task");
    nico.update_context(
        id,
        ContextUpdate {
            artifacts: vec![Artifact {
                subject: format!("patch:{PRODUCED}"),
                role: ArtifactRole::Produced,
                purpose: "forged".to_owned(),
            }],
            ..ContextUpdate::default()
        },
    )
    .await
    .expect_err("the member cannot invent a produced patch");
    assert!(
        nico.get(id)
            .await
            .expect("task should remain")
            .context
            .patch()
            .is_none()
    );
}

#[tokio::test]
async fn an_explicit_stop_survives_an_interrupted_process() {
    let queue = queue().await;
    let id = queue
        .push(crate::queue::TaskRequest::new(
            "scaffold".to_owned(),
            subject(),
            QueuePosition::End,
        ))
        .await
        .expect("request should record")
        .task;
    queue.next().await.expect("task should start");
    queue.cancel(id).await.expect("stop should persist");
    queue.recover().await.expect("task should recover");
    assert!(
        queue
            .next()
            .await
            .expect("task should resume for preservation")
            .context
            .cancelled
    );
}

#[tokio::test]
async fn assignment_and_receipt_commit_together_and_follow_task_completion() {
    use crate::{
        delivery::State,
        watcher::{Activity, Kind},
    };
    let queue = queue().await;
    let inbox = queue.inbox().await.expect("inbox should open");
    let activity = Activity {
        repository: REPOSITORY.parse().expect("RID"),
        subject: Subject::Issue(SUBJECT.parse().expect("issue")),
        operation: PRODUCED.parse().expect("operation"),
        action: 0,
        author: "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            .parse()
            .expect("DID"),
        kind: Kind::Assigned,
    };
    let request = || {
        let mut request =
            crate::queue::TaskRequest::new("assigned".into(), subject(), QueuePosition::End);
        request.request = Some("assignment".into());
        request
    };
    queue
        .database()
        .execute(
            "CREATE TRIGGER refuse_receipt BEFORE INSERT ON delivery_inbox BEGIN SELECT \
             RAISE(ABORT, 'test failure'); END;",
        )
        .await
        .expect("failure trigger");
    queue
        .push_assignment(request(), activity.clone())
        .await
        .expect_err("receipt failure must reject admission");
    assert!(
        queue.pending().await.expect("queue").is_empty(),
        "a task cannot exist without its admitted receipt"
    );
    queue
        .database()
        .execute("DROP TRIGGER refuse_receipt")
        .await
        .expect("allow admission");
    let admitted = queue
        .push_assignment(request(), activity.clone())
        .await
        .expect("admit");
    assert_eq!(
        inbox.list().await.expect("inbox")[0].task,
        Some(admitted.task)
    );
    queue.next().await.expect("start task");
    queue.finish().await.expect("finish task");
    assert_eq!(inbox.list().await.expect("inbox")[0].state, State::Handled);
    queue
        .push_assignment(request(), activity.clone())
        .await
        .expect("restart scan");
    assert!(
        queue.pending().await.expect("queue").is_empty(),
        "completed assignment cannot start again"
    );
    assert_eq!(inbox.list().await.expect("inbox").len(), 1);
    let cancelled = || {
        let mut request = request();
        request.request = Some("cancelled-assignment".into());
        request
    };
    let cancelled_id = queue.push(cancelled()).await.expect("record task").task;
    queue.cancel(cancelled_id).await.expect("cancel task");
    let mut activity = activity;
    activity.operation = "4444444444444444444444444444444444444444"
        .parse()
        .expect("operation");
    let receipt = queue
        .push_assignment(cancelled(), activity)
        .await
        .expect("rescan cancelled assignment");
    assert_eq!(receipt.state, crate::queue::TaskState::Cancelled);
    let records = inbox.list().await.expect("inbox");
    let record = records
        .iter()
        .find(|record| record.task == Some(cancelled_id))
        .expect("cancelled receipt");
    assert_eq!(record.state, State::Ignored);
    assert!(
        record
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("cancelled"))
    );
    assert!(queue.pending().await.expect("queue").is_empty());
}
