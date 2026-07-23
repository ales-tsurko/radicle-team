//! MCP integration and repository-service tests.

use std::str::FromStr as _;
#[cfg(not(miri))]
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use radicle::{
    cob::{
        EntryId, Manifest, Op, Timestamp, Version,
        patch::{Action as PatchAction, TYPENAME},
    },
    crypto::PublicKey,
    git::Oid,
};
#[cfg(not(miri))]
use radicle::{
    cob::{
        ObjectId,
        issue::{self, Issues},
        patch::{self, Patches},
        store::access::ReadOnly,
    },
    crypto::Seed,
    identity::{RepoId, doc::Visibility},
    node::Alias,
    profile::{Home, Profile},
    storage::{ReadRepository as _, ReadStorage as _},
};

#[cfg(not(miri))]
use crate::mcp::{
    protocol::{
        CommentOperation, CommentTargetKind, CommentWriteRequest, CreateIssueRequest,
        CreatePatchRequest, DiffRequest, Document, EditIssueRequest, EditPatchRequest,
        LocationRequest, MergePatchRequest, ResolveCommentRequest, ReviewWriteRequest,
        SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind, SubjectRequest,
        mutation_identity, parse_location, parse_object, stringify, validate_code_location,
    },
    read::MAX_DIFF_BYTES,
    recovery::review_mutation_operation,
    server::Server,
};
#[cfg(not(miri))]
use crate::mcp::server::ServiceError;
use crate::mcp::{
    read::{paginate_operations, revision_action},
    tools::ToolService,
};

#[cfg(not(miri))]
const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

#[cfg(not(miri))]
struct RepositoryFixture {
    _root: tempfile::TempDir,
    profile: Profile,
    repository: RepoId,
    working: std::path::PathBuf,
    base: git2::Oid,
    head: git2::Oid,
}

#[cfg(not(miri))]
impl RepositoryFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = Home::new(root.path().join("home")).expect("home should be created");
        let profile = Profile::init(home, Alias::new("mcp-fixture"), None, Seed::generate())
            .expect("profile should be initialized");
        let working = root.path().join("working");
        let checkout = git2::Repository::init(&working).expect("repository should initialize");
        let signature =
            git2::Signature::now("MCP Test", "mcp@example.com").expect("signature should be valid");

        fs::write(working.join("README.md"), "# Test\n").expect("README should be written");
        let base = commit(&checkout, &signature, "Initial commit", &[]);
        fs::write(working.join("README.md"), "# Test\nChanged\n")
            .expect("README should be updated");
        let parent = checkout.find_commit(base).expect("base should exist");
        let head = commit(&checkout, &signature, "Change README", &[&parent]);
        drop(parent);

        let signer = profile.signer().expect("signer should load");
        let (repository, _document, _refs) = radicle::rad::init(
            &checkout,
            "mcp-fixture"
                .try_into()
                .expect("project name should be valid"),
            "MCP fixture",
            "main".try_into().expect("branch should be valid"),
            Visibility::default(),
            &signer,
            &profile.storage,
        )
        .expect("Radicle repository should initialize");

        Self {
            _root: root,
            profile,
            repository,
            working,
            base,
            head,
        }
    }

    fn service(&self) -> ToolService {
        let publisher = crate::agent::Publisher::new(
            self.profile.clone(),
            self.repository,
            None,
            TestAnnouncer,
            TestAuthorizer,
        );
        ToolService::new(
            self.profile.storage.clone(),
            self.repository,
            super::DeliveryScope::default(),
            publisher,
            crate::manifest::McpAllowlist::default(),
        )
    }

    async fn journaled_service(
        &self,
        scope: super::DeliveryScope,
        announcer: impl crate::agent::Announcer,
    ) -> ToolService {
        let publisher = crate::agent::Publisher::new(
            self.profile.clone(),
            self.repository,
            None,
            announcer,
            TestAuthorizer,
        )
        .with_memory_journal()
        .await
        .expect("mutation journal should initialize");
        ToolService::new(
            self.profile.storage.clone(),
            self.repository,
            scope,
            publisher,
            crate::manifest::McpAllowlist::default(),
        )
    }

    fn subject(kind: SubjectKind, id: impl ToString) -> SubjectRequest {
        SubjectRequest {
            kind,
            subject: id.to_string(),
        }
    }
}

#[cfg(not(miri))]
fn commit(
    repository: &git2::Repository,
    signature: &git2::Signature<'_>,
    message: &str,
    parents: &[&git2::Commit<'_>],
) -> git2::Oid {
    let mut index = repository.index().expect("index should open");
    index
        .add_path(Path::new("README.md"))
        .expect("README should be added");
    let tree = index.write_tree().expect("tree should be written");
    let tree = repository.find_tree(tree).expect("tree should exist");
    repository
        .commit(
            Some("refs/heads/main"),
            signature,
            signature,
            message,
            &tree,
            parents,
        )
        .expect("commit should be created")
}

#[cfg(not(miri))]
fn result_id(result: rmcp::Json<Document>) -> String {
    result
        .0
        .data
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("write result should contain an id")
        .to_owned()
}

#[cfg(not(miri))]
fn object_count(repository: &git2::Repository) -> usize {
    let mut count = 0;
    repository
        .odb()
        .expect("object database should open")
        .foreach(|_| {
            count += 1;
            true
        })
        .expect("objects should be enumerable");
    count
}

#[test]
#[cfg(not(miri))]
fn mutation_identity_is_stable_and_payload_sensitive() {
    let request = CreateIssueRequest {
        title: "Bug".to_owned(),
        description: "Description".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let identity = mutation_identity("create_issue", &request).expect("request should hash");

    assert_eq!(
        identity,
        mutation_identity("create_issue", &request).expect("request should hash")
    );
    assert_ne!(
        identity,
        mutation_identity("edit_issue", &request).expect("request should hash")
    );
    assert_ne!(
        identity,
        mutation_identity(
            "create_issue",
            &CreateIssueRequest {
                description: "Different".to_owned(),
                ..request
            }
        )
        .expect("request should hash")
    );
}

#[test]
fn exposes_the_complete_mcp_tool_set() {
    let names = ToolService::registered_tools();

    assert!(!names.is_empty());
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
}

#[tokio::test]
#[cfg(not(miri))]
async fn starts_on_a_loopback_capability_url_and_stops() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let home = Home::new(root.path()).expect("Radicle home should be created");
    let profile = Profile::init(home, Alias::new("mcp-test"), None, Seed::generate())
        .expect("Radicle profile should be initialized");
    let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
    let publisher = crate::agent::Publisher::new(
        profile.clone(),
        repository,
        None,
        TestAnnouncer,
        TestAuthorizer,
    );
    let server = Server::start(
        profile.storage.clone(),
        repository,
        publisher,
        crate::manifest::McpAllowlist::default(),
    )
    .await
    .expect("MCP server should start");

    assert_eq!(server.endpoint().url().host_str(), Some("127.0.0.1"));
    assert!(server.endpoint().url().path().starts_with("/mcp/"));
    server.stop().await.expect("MCP server should stop");
}

#[tokio::test]
#[cfg(not(miri))]
async fn limits_tools_to_the_active_delivery_subject() {
    let scope = super::DeliveryScope::default();
    let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
    let subject = radicle::cob::ObjectId::from_str("1111111111111111111111111111111111111111")
        .expect("subject should be valid");
    let operation = EntryId::from_str("2222222222222222222222222222222222222222")
        .expect("operation should be valid");
    let _inactive = scope
        .access_current()
        .await
        .expect_err("inactive scope should reject access");

    scope
        .activate(crate::agent::Context {
            repository,
            subject: crate::watcher::Subject::Issue(subject),
            delivery: crate::agent::DeliveryId {
                operation,
                action: 0,
            },
            author: radicle::identity::Did::from(
                PublicKey::from_str(AUTHOR).expect("author should be valid"),
            ),
            workspace: "/tmp/delivery".into(),
        })
        .await
        .expect("delivery should activate");
    let access = scope
        .access(&SubjectRequest {
            kind: SubjectKind::Issue,
            subject: subject.to_string(),
        })
        .await
        .expect("active issue should be accessible");
    drop(access);
    let _wrong_subject = scope
        .access(&SubjectRequest {
            kind: SubjectKind::Patch,
            subject: subject.to_string(),
        })
        .await
        .expect_err("different subject kind should be rejected");
    scope.deactivate().await;
    let _inactive = scope
        .access_current()
        .await
        .expect_err("deactivated scope should reject access");
}

#[cfg(not(miri))]
#[derive(Debug, Clone, Copy)]
struct TestAnnouncer;

#[cfg(not(miri))]
impl crate::agent::Announcer for TestAnnouncer {
    fn announce(
        &self,
        _repository: RepoId,
        _namespace: PublicKey,
    ) -> Result<(), crate::agent::PublishError> {
        Ok(())
    }
}

#[cfg(not(miri))]
#[derive(Debug, Clone, Copy)]
struct TestAuthorizer;

#[cfg(not(miri))]
#[async_trait::async_trait]
impl crate::agent::Authorizer for TestAuthorizer {
    async fn authorize(&self, _method: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(not(miri))]
#[derive(Debug, Clone, Copy)]
struct DenyAuthorizer;

#[cfg(not(miri))]
#[async_trait::async_trait]
impl crate::agent::Authorizer for DenyAuthorizer {
    async fn authorize(&self, _method: &str) -> Result<(), String> {
        Err("writes disabled".to_owned())
    }
}

#[cfg(not(miri))]
#[derive(Debug, Clone)]
struct FlakyAnnouncer(Arc<AtomicUsize>);

#[cfg(not(miri))]
impl crate::agent::Announcer for FlakyAnnouncer {
    fn announce(
        &self,
        _repository: RepoId,
        _namespace: PublicKey,
    ) -> Result<(), crate::agent::PublishError> {
        if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            Err(crate::agent::PublishError::Task(
                "simulated announcement failure".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
#[cfg(not(miri))]
async fn recovers_a_completed_write_when_the_delivery_is_replayed() {
    let fixture = RepositoryFixture::new();
    let scope = super::DeliveryScope::default();
    let announcements = Arc::new(AtomicUsize::new(0));
    let service = fixture
        .journaled_service(scope.clone(), FlakyAnnouncer(Arc::clone(&announcements)))
        .await;
    let subject = radicle::cob::ObjectId::from_str("1111111111111111111111111111111111111111")
        .expect("subject should be valid");
    let context = crate::agent::Context {
        repository: fixture.repository,
        subject: crate::watcher::Subject::Issue(subject),
        delivery: crate::agent::DeliveryId {
            operation: EntryId::from_str("2222222222222222222222222222222222222222")
                .expect("operation should be valid"),
            action: 0,
        },
        author: fixture.profile.did(),
        workspace: fixture.working.clone(),
    };
    let request = || CreateIssueRequest {
        title: "Recovered bug".to_owned(),
        description: "Created once".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let mutation = crate::agent::MutationKey {
        delivery: context.delivery,
        invocation: 0,
        request: mutation_identity("create_issue", &request()).expect("request should serialize"),
    };

    scope
        .activate(context.clone())
        .await
        .expect("delivery should activate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    assert!(
        delivery_service
            .create_issue_write(request())
            .await
            .is_err()
    );
    drop(access);
    service
        .publisher
        .forget_completion(&mutation)
        .await
        .expect("completion should be cleared to simulate an interrupted journal update");
    service
        .publisher
        .reconcile(context.delivery)
        .await
        .expect("unfinished write should reconcile without an exact retry");
    service
        .publisher
        .forget_completion(&mutation)
        .await
        .expect("completion should be cleared to simulate an interrupted journal update");
    scope.deactivate().await;
    scope
        .activate(context)
        .await
        .expect("delivery should reactivate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    service
        .for_delivery(&access)
        .create_issue_write(request())
        .await
        .expect("replayed write should recover");
    drop(access);

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        radicle::cob::issue::Issues::open(&repository, radicle::cob::store::access::ReadOnly,)
            .expect("issues should open")
            .count()
            .expect("issues should count"),
        1
    );
    assert_eq!(announcements.load(Ordering::Relaxed), 3);
}

#[tokio::test]
#[cfg(not(miri))]
async fn recovers_an_unsigned_local_write_without_duplication() {
    let fixture = RepositoryFixture::new();
    let scope = super::DeliveryScope::default();
    let service = fixture
        .journaled_service(scope.clone(), TestAnnouncer)
        .await;
    let context = crate::agent::Context {
        repository: fixture.repository,
        subject: crate::watcher::Subject::Issue(
            ObjectId::from_str("1111111111111111111111111111111111111111")
                .expect("subject should be valid"),
        ),
        delivery: crate::agent::DeliveryId {
            operation: EntryId::from_str("2222222222222222222222222222222222222222")
                .expect("operation should be valid"),
            action: 0,
        },
        author: fixture.profile.did(),
        workspace: fixture.working.clone(),
    };
    let request = || CreateIssueRequest {
        title: "Interrupted bug".to_owned(),
        description: "Created before signing".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let mutation = crate::agent::MutationKey {
        delivery: context.delivery,
        invocation: 0,
        request: mutation_identity("create_issue", &request()).expect("request should serialize"),
    };
    scope
        .activate(context)
        .await
        .expect("delivery should activate");

    let interrupted = service
        .publisher
        .mutate::<ObjectId, _, _>(
            "create_issue",
            Some(mutation),
            move |repository, signer, marker| {
                let identity = repository.identity_head().map_err(stringify)?;
                let mut issues = radicle::cob::store::Store::<issue::Issue, _, _>::open(
                    repository,
                    radicle::cob::store::access::WriteAs::new(signer),
                )
                .map_err(stringify)?
                .identity(identity);
                let _created = radicle::cob::store::Transaction::initial(
                    marker,
                    &mut issues,
                    |tx: &mut radicle::cob::store::Transaction<issue::Issue, _>, _repository| {
                        tx.push(issue::Action::Comment {
                            body: "Created before signing".to_owned(),
                            reply_to: None,
                            embeds: Vec::new(),
                        })?;
                        tx.push(issue::Action::Edit {
                            title: radicle::cob::Title::try_from("Interrupted bug".to_owned())
                                .expect("title should be valid"),
                        })
                    },
                )
                .map_err(stringify)?;
                Err("simulated crash before signing".to_owned())
            },
            move |repository, _author, operation, _validation| {
                Issues::open(repository, ReadOnly)
                    .map_err(stringify)?
                    .get(&ObjectId::from(operation))
                    .map(|issue| issue.is_some())
                    .map_err(stringify)
            },
        )
        .await;
    interrupted.expect_err("the simulated crash should interrupt publication");

    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    service
        .for_delivery(&access)
        .create_issue_write(request())
        .await
        .expect("unsigned write should recover");
    drop(access);

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        Issues::open(&repository, ReadOnly)
            .expect("issues should open")
            .count()
            .expect("issues should count"),
        1
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn recovers_created_comments_and_reviews_by_operation_id() {
    let fixture = RepositoryFixture::new();
    let scope = super::DeliveryScope::default();
    let service = fixture
        .journaled_service(scope.clone(), TestAnnouncer)
        .await;
    let delivery = crate::agent::DeliveryId {
        operation: EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should be valid"),
        action: 0,
    };
    let context = crate::agent::Context {
        repository: fixture.repository,
        subject: crate::watcher::Subject::Issue(
            ObjectId::from_str("1111111111111111111111111111111111111111")
                .expect("subject should be valid"),
        ),
        delivery,
        author: fixture.profile.did(),
        workspace: fixture.working.clone(),
    };
    scope
        .activate(context.clone())
        .await
        .expect("delivery should activate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);

    let issue_request = || CreateIssueRequest {
        title: "Recovery target".to_owned(),
        description: "Issue".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let issue = result_id(
        delivery_service
            .create_issue_write(issue_request())
            .await
            .expect("issue should be created"),
    );
    let comment_request = || CommentWriteRequest {
        subject: SubjectRequest {
            kind: SubjectKind::Issue,
            subject: issue.clone(),
        },
        target: CommentTargetKind::Issue,
        entity: None,
        comment: Some("3333333333333333333333333333333333333333".to_owned()),
        body: Some("Created comment".to_owned()),
        reply_to: None,
        location: None,
    };
    delivery_service
        .write_comment(comment_request(), CommentOperation::Create)
        .await
        .expect("comment should be created");
    service
        .publisher
        .forget_completion(&crate::agent::MutationKey {
            delivery,
            invocation: 0,
            request: mutation_identity("create_comment", &comment_request())
                .expect("request should serialize"),
        })
        .await
        .expect("comment completion should be cleared");
    drop(access);
    scope.deactivate().await;
    scope
        .activate(context.clone())
        .await
        .expect("delivery should reactivate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    delivery_service
        .create_issue_write(issue_request())
        .await
        .expect("issue should recover");
    delivery_service
        .write_comment(comment_request(), CommentOperation::Create)
        .await
        .expect("comment should recover");

    let patch_request = || CreatePatchRequest {
        title: "Review target".to_owned(),
        description: "Patch".to_owned(),
        base: fixture.base.to_string(),
        head: fixture.head.to_string(),
        labels: Vec::new(),
        draft: false,
    };
    let patch = result_id(
        delivery_service
            .create_patch_write(patch_request(), fixture.working.clone())
            .await
            .expect("patch should be created"),
    );
    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let patch_id = ObjectId::from_str(&patch).expect("patch should parse");
    let patch_value = Patches::open(&repository, ReadOnly)
        .expect("patches should open")
        .get(&patch_id)
        .expect("patch should load")
        .expect("patch should exist");
    let revision = patch_value.root().0.to_string();
    drop(repository);
    let review_request = || ReviewWriteRequest {
        subject: SubjectRequest {
            kind: SubjectKind::Patch,
            subject: patch.clone(),
        },
        revision: Some(revision.clone()),
        review: Some("4444444444444444444444444444444444444444".to_owned()),
        verdict: Some("accept".to_owned()),
        summary: Some("Looks good".to_owned()),
        labels: Vec::new(),
    };
    delivery_service
        .review_write(review_request(), false)
        .await
        .expect("review should be created");
    service
        .publisher
        .forget_completion(&crate::agent::MutationKey {
            delivery,
            invocation: 0,
            request: mutation_identity("create_review", &review_request())
                .expect("request should serialize"),
        })
        .await
        .expect("review completion should be cleared");
    drop(access);
    scope.deactivate().await;
    scope
        .activate(context)
        .await
        .expect("delivery should reactivate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    delivery_service
        .create_issue_write(issue_request())
        .await
        .expect("issue should recover");
    delivery_service
        .write_comment(comment_request(), CommentOperation::Create)
        .await
        .expect("comment should recover");
    delivery_service
        .create_patch_write(patch_request(), fixture.working.clone())
        .await
        .expect("patch should recover");
    delivery_service
        .review_write(review_request(), false)
        .await
        .expect("review should recover");
    drop(access);

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let issue_value = Issues::open(&repository, ReadOnly)
        .expect("issues should open")
        .get(&ObjectId::from_str(&issue).expect("issue should parse"))
        .expect("issue should load")
        .expect("issue should exist");
    assert_eq!(issue_value.comments().count(), 2);
    let patch_value = Patches::open(&repository, ReadOnly)
        .expect("patches should open")
        .get(&patch_id)
        .expect("patch should load")
        .expect("patch should exist");
    assert_eq!(
        patch_value
            .reviews_of(patch::RevisionId::from(
                EntryId::from_str(&revision).expect("revision should parse")
            ))
            .count(),
        1
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn replays_stale_mutations_and_distinguishes_repeated_invocations() {
    let fixture = RepositoryFixture::new();
    let scope = super::DeliveryScope::default();
    let service = fixture
        .journaled_service(scope.clone(), TestAnnouncer)
        .await;
    let delivery = crate::agent::DeliveryId {
        operation: EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should be valid"),
        action: 0,
    };
    let context = crate::agent::Context {
        repository: fixture.repository,
        subject: crate::watcher::Subject::Issue(
            ObjectId::from_str("1111111111111111111111111111111111111111")
                .expect("subject should be valid"),
        ),
        delivery,
        author: fixture.profile.did(),
        workspace: fixture.working.clone(),
    };
    scope
        .activate(context.clone())
        .await
        .expect("delivery should activate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    let issue_request = || CreateIssueRequest {
        title: "Original".to_owned(),
        description: "Issue".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let issue = result_id(
        delivery_service
            .create_issue_write(issue_request())
            .await
            .expect("issue should be created"),
    );
    let subject = SubjectRequest {
        kind: SubjectKind::Issue,
        subject: issue.clone(),
    };
    let stale = || EditIssueRequest {
        subject: subject.clone(),
        title: Some("Stale".to_owned()),
        description: None,
    };
    delivery_service
        .edit_issue_write(stale())
        .await
        .expect("first edit should apply");
    service
        .publisher
        .forget_completion(&crate::agent::MutationKey {
            delivery,
            invocation: 0,
            request: mutation_identity("edit_issue", &stale()).expect("request should serialize"),
        })
        .await
        .expect("edit completion should be cleared");
    service
        .edit_issue_write(EditIssueRequest {
            subject: subject.clone(),
            title: Some("Newer".to_owned()),
            description: None,
        })
        .await
        .expect("newer edit should apply");
    drop(access);
    scope.deactivate().await;
    scope
        .activate(context)
        .await
        .expect("delivery should reactivate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    delivery_service
        .create_issue_write(issue_request())
        .await
        .expect("issue creation should recover");
    delivery_service
        .edit_issue_write(stale())
        .await
        .expect("stale edit should recover without replay");
    let stale_labels = || SetStringsRequest {
        subject: subject.clone(),
        values: vec!["stale".to_owned()],
    };
    delivery_service
        .set_strings_write(stale_labels(), SubjectKind::Issue, StringUpdate::Labels)
        .await
        .expect("first label update should apply");
    delivery_service
        .set_strings_write(
            SetStringsRequest {
                subject: subject.clone(),
                values: vec!["newer".to_owned()],
            },
            SubjectKind::Issue,
            StringUpdate::Labels,
        )
        .await
        .expect("newer labels should apply");
    delivery_service
        .set_strings_write(stale_labels(), SubjectKind::Issue, StringUpdate::Labels)
        .await
        .expect("stale labels should recover without replay");
    drop(access);

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let issue = Issues::open(&repository, ReadOnly)
        .expect("issues should open")
        .get(&ObjectId::from_str(&issue).expect("issue should parse"))
        .expect("issue should load")
        .expect("issue should exist");
    assert_eq!(issue.title(), "Newer");
    assert_eq!(
        issue.labels().map(ToString::to_string).collect::<Vec<_>>(),
        ["stale"]
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn distinguishes_identical_mutations_and_replays_concurrent_writes() {
    let fixture = RepositoryFixture::new();
    let scope = super::DeliveryScope::default();
    let service = fixture
        .journaled_service(scope.clone(), TestAnnouncer)
        .await;
    let subject = radicle::cob::ObjectId::from_str("1111111111111111111111111111111111111111")
        .expect("subject should be valid");
    let context = crate::agent::Context {
        repository: fixture.repository,
        subject: crate::watcher::Subject::Issue(subject),
        delivery: crate::agent::DeliveryId {
            operation: EntryId::from_str("2222222222222222222222222222222222222222")
                .expect("operation should be valid"),
            action: 0,
        },
        author: fixture.profile.did(),
        workspace: fixture.working.clone(),
    };
    scope
        .activate(context.clone())
        .await
        .expect("delivery should activate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_delivery(&access);
    let deactivating = tokio::spawn({
        let scope = scope.clone();
        async move { scope.deactivate().await }
    });
    tokio::task::yield_now().await;
    let request = || CreateIssueRequest {
        title: "Concurrent bug".to_owned(),
        description: "Created once".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };

    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(
            delivery_service.create_issue_write(request()),
            delivery_service.create_issue_write(request())
        )
    })
    .await
    .expect("queued deactivation must not deadlock mutation-key creation");
    let distinct = |title: &str| CreateIssueRequest {
        title: title.to_owned(),
        description: format!("{title} body"),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let (alpha, beta) = tokio::join!(
        delivery_service.create_issue_write(distinct("Alpha")),
        delivery_service.create_issue_write(distinct("Beta"))
    );
    let first = result_id(first.expect("first write should succeed"));
    let second = result_id(second.expect("second write should succeed"));
    let alpha = result_id(alpha.expect("alpha write should succeed"));
    let beta = result_id(beta.expect("beta write should succeed"));
    drop(access);
    deactivating
        .await
        .expect("deactivation task should complete");
    assert_ne!(first, second);

    scope
        .activate(context)
        .await
        .expect("delivery should reactivate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let replay = service.for_delivery(&access);
    assert_eq!(
        result_id(
            replay
                .create_issue_write(distinct("Beta"))
                .await
                .expect("beta should recover")
        ),
        beta
    );
    assert_eq!(
        result_id(
            replay
                .create_issue_write(distinct("Alpha"))
                .await
                .expect("alpha should recover")
        ),
        alpha
    );
    let replayed_first = result_id(
        replay
            .create_issue_write(request())
            .await
            .expect("first identical write should recover"),
    );
    let replayed_second = result_id(
        replay
            .create_issue_write(request())
            .await
            .expect("second identical write should recover"),
    );
    assert_eq!(
        std::collections::BTreeSet::from([replayed_first, replayed_second]),
        std::collections::BTreeSet::from([first, second])
    );
    drop(access);

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        radicle::cob::issue::Issues::open(&repository, radicle::cob::store::access::ReadOnly)
            .expect("issues should open")
            .count()
            .expect("issues should count"),
        4
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn preserves_and_signs_workspace_only_patch_commits() {
    use radicle::storage::refs::SignedRefs;

    let fixture = RepositoryFixture::new();
    let checkout = git2::Repository::open(&fixture.working).expect("checkout should open");
    fs::write(fixture.working.join("README.md"), "# Test\nChanged again\n")
        .expect("README should update");
    let signature =
        git2::Signature::now("MCP Test", "mcp@example.com").expect("signature should be valid");
    let parent = checkout
        .find_commit(fixture.head)
        .expect("parent should exist");
    let head = commit(&checkout, &signature, "Workspace-only change", &[&parent]);
    drop(parent);
    let service = fixture.service();

    service
        .create_patch_write(
            CreatePatchRequest {
                title: "Workspace change".to_owned(),
                description: "Retain imported commits".to_owned(),
                base: fixture.head.to_string(),
                head: head.to_string(),
                labels: Vec::new(),
                draft: false,
            },
            fixture.working.clone(),
        )
        .await
        .expect("patch should be created");

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let name = format!("refs/radicle-team/commits/{head}");
    let qualified = format!("refs/namespaces/{}/{}", fixture.profile.id(), name);
    assert_eq!(
        repository
            .backend
            .find_reference(&qualified)
            .expect("retention ref should exist")
            .target(),
        Some(head)
    );
    let signed = SignedRefs::load(*fixture.profile.id(), &repository)
        .expect("signed refs should load")
        .expect("signed refs should exist");
    assert!(
        signed
            .iter()
            .any(|(reference, oid)| reference.as_str() == name
                && oid.to_string() == head.to_string())
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn migrates_legacy_pending_replies_without_duplicates() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service();
    let issue = result_id(
        service
            .create_issue_write(CreateIssueRequest {
                title: "Legacy recovery".to_owned(),
                description: "Issue description".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("issue should be created"),
    );
    let issue = parse_object(&issue).expect("issue id should parse");
    let delivery = crate::agent::DeliveryId {
        operation: EntryId::from_str("2222222222222222222222222222222222222222")
            .expect("operation should parse"),
        action: 4,
    };
    let reply = crate::agent::LegacyReply {
        body: "Reply from the previous runtime".to_owned(),
        target: crate::agent::LegacyReplyTarget::Issue { reply_to: None },
    };
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        TestAnnouncer,
        TestAuthorizer,
    );
    publisher
        .reconcile_legacy(
            crate::watcher::Subject::Issue(issue),
            delivery,
            *fixture.profile.id(),
            &reply,
        )
        .await
        .expect("authorized legacy reply should publish");
    let denied = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        TestAnnouncer,
        DenyAuthorizer,
    );
    denied
        .reconcile_legacy(
            crate::watcher::Subject::Issue(issue),
            delivery,
            *fixture.profile.id(),
            &reply,
        )
        .await
        .expect("an existing legacy operation should recover without reauthorization");
    denied
        .reconcile_legacy(
            crate::watcher::Subject::Issue(issue),
            crate::agent::DeliveryId {
                operation: delivery.operation,
                action: delivery.action + 1,
            },
            *fixture.profile.id(),
            &crate::agent::LegacyReply {
                body: "Unauthorized fallback".to_owned(),
                target: reply.target,
            },
        )
        .await
        .expect_err("a new legacy operation should require current authorization");

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let issue =
        radicle::cob::issue::Issues::open(&repository, radicle::cob::store::access::ReadOnly)
            .expect("issues should open")
            .get(&issue)
            .expect("issue should load")
            .expect("issue should exist");
    assert_eq!(issue.replies().count(), 1);
}

#[tokio::test]
#[cfg(not(miri))]
async fn manages_issues_and_their_comments() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service();
    let issue = result_id(
        service
            .create_issue_write(CreateIssueRequest {
                title: "Bug".to_owned(),
                description: "Original".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("issue should be created"),
    );
    let subject = RepositoryFixture::subject(SubjectKind::Issue, &issue);

    service
        .set_state_write(
            SetStateRequest {
                subject: subject.clone(),
                state: "closed".to_owned(),
            },
            SubjectKind::Issue,
        )
        .await
        .expect("issue state should update");
    service
        .set_strings_write(
            SetStringsRequest {
                subject: subject.clone(),
                values: vec!["bug".to_owned(), "bug".to_owned()],
            },
            SubjectKind::Issue,
            StringUpdate::Labels,
        )
        .await
        .expect("issue labels should update");
    service
        .set_strings_write(
            SetStringsRequest {
                subject: subject.clone(),
                values: vec![
                    fixture.profile.did().to_string(),
                    fixture.profile.did().to_string(),
                ],
            },
            SubjectKind::Issue,
            StringUpdate::Assignees,
        )
        .await
        .expect("issue assignees should update");

    service
        .edit_issue_write(EditIssueRequest {
            subject: subject.clone(),
            title: Some("Fixed title".to_owned()),
            description: Some("Fixed description".to_owned()),
        })
        .await
        .expect("issue should be edited");
    assert!(
        service
            .edit_issue_write(EditIssueRequest {
                subject: subject.clone(),
                title: None,
                description: None,
            })
            .await
            .is_err()
    );

    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let objects = object_count(&repository.backend);
    for _attempt in 0..2 {
        service
            .write_comment(
                CommentWriteRequest {
                    subject: subject.clone(),
                    target: CommentTargetKind::Issue,
                    entity: None,
                    comment: None,
                    body: Some("Invalid reply".to_owned()),
                    reply_to: Some("8888888888888888888888888888888888888888".to_owned()),
                    location: None,
                },
                CommentOperation::Create,
            )
            .await
            .err()
            .expect("a missing issue reply parent should be rejected");
    }
    assert_eq!(
        object_count(&repository.backend),
        objects,
        "invalid issue replies must not write Git objects"
    );

    let comment = result_id(
        service
            .write_comment(
                CommentWriteRequest {
                    subject: subject.clone(),
                    target: CommentTargetKind::Issue,
                    entity: None,
                    comment: None,
                    body: Some("Finding".to_owned()),
                    reply_to: None,
                    location: None,
                },
                CommentOperation::Create,
            )
            .await
            .expect("comment should be created"),
    );
    service
        .write_comment(
            CommentWriteRequest {
                subject: subject.clone(),
                target: CommentTargetKind::Issue,
                entity: None,
                comment: Some(comment.clone()),
                body: Some("Updated finding".to_owned()),
                reply_to: None,
                location: None,
            },
            CommentOperation::Edit,
        )
        .await
        .expect("comment should be edited");
    service
        .write_comment(
            CommentWriteRequest {
                subject: subject.clone(),
                target: CommentTargetKind::Issue,
                entity: None,
                comment: Some(comment.clone()),
                body: None,
                reply_to: None,
                location: None,
            },
            CommentOperation::Redact,
        )
        .await
        .expect("comment should be redacted");
    for operation in [CommentOperation::Edit, CommentOperation::Redact] {
        assert!(
            service
                .write_comment(
                    CommentWriteRequest {
                        subject: subject.clone(),
                        target: CommentTargetKind::Issue,
                        entity: None,
                        comment: Some(comment.clone()),
                        body: matches!(operation, CommentOperation::Edit)
                            .then(|| "Too late".to_owned()),
                        reply_to: None,
                        location: None,
                    },
                    operation,
                )
                .await
                .is_err(),
            "redacted comments must not be mutated"
        );
    }
    service
        .publisher
        .reconcile_legacy(
            crate::watcher::Subject::Issue(
                ObjectId::from_str(&subject.subject).expect("issue should parse"),
            ),
            crate::agent::DeliveryId {
                operation: EntryId::from_str("9999999999999999999999999999999999999999")
                    .expect("operation should parse"),
                action: 0,
            },
            *fixture.profile.id(),
            &crate::agent::LegacyReply {
                body: "Stale recovery".to_owned(),
                target: crate::agent::LegacyReplyTarget::Issue {
                    reply_to: Some(EntryId::from_str(&comment).expect("comment should parse")),
                },
            },
        )
        .await
        .expect("an unavailable legacy target should be discarded");
}

#[tokio::test]
#[cfg(not(miri))]
async fn manages_patch_reviews_and_inline_comments() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service();
    let patch = result_id(
        service
            .create_patch_write(
                CreatePatchRequest {
                    title: "Change".to_owned(),
                    description: "Original revision".to_owned(),
                    base: fixture.base.to_string(),
                    head: fixture.head.to_string(),
                    labels: Vec::new(),
                    draft: false,
                },
                fixture.working.clone(),
            )
            .await
            .expect("patch should be created"),
    );
    let subject = RepositoryFixture::subject(SubjectKind::Patch, &patch);
    service
        .set_state_write(
            SetStateRequest {
                subject: subject.clone(),
                state: "draft".to_owned(),
            },
            SubjectKind::Patch,
        )
        .await
        .expect("patch state should update");
    service
        .set_strings_write(
            SetStringsRequest {
                subject: subject.clone(),
                values: vec!["review".to_owned(), "review".to_owned()],
            },
            SubjectKind::Patch,
            StringUpdate::Labels,
        )
        .await
        .expect("patch labels should update");
    service
        .set_strings_write(
            SetStringsRequest {
                subject: subject.clone(),
                values: vec![
                    fixture.profile.did().to_string(),
                    fixture.profile.did().to_string(),
                ],
            },
            SubjectKind::Patch,
            StringUpdate::Assignees,
        )
        .await
        .expect("patch assignees should update");
    let repository = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let patch_id = parse_object(&patch).expect("patch id should be valid");
    let patch_state =
        radicle::cob::patch::Patches::open(&repository, radicle::cob::store::access::ReadOnly)
            .expect("patch store should open")
            .get(&patch_id)
            .expect("patch should load")
            .expect("patch should exist");
    let (revision, revision_state) = patch_state.root();
    let revision = revision.to_string();

    let diff = service
        .diff(DiffRequest {
            subject: subject.clone(),
            revision: Some(revision.clone()),
        })
        .expect("diff should load");
    assert!(diff.files.iter().any(|file| {
        file.new_path.as_deref() == Some("README.md")
            && file.lines.iter().any(|line| line.new_line == Some(2))
    }));
    let location = LocationRequest {
        commit: fixture.head.to_string(),
        path: "README.md".to_owned(),
        old_start: None,
        old_end: None,
        new_start: Some(2),
        new_end: Some(3),
    };
    let parsed_location = parse_location(LocationRequest {
        commit: location.commit.clone(),
        path: location.path.clone(),
        old_start: location.old_start,
        old_end: location.old_end,
        new_start: location.new_start,
        new_end: location.new_end,
    })
    .expect("location should parse");
    validate_code_location(&repository, revision_state, &parsed_location)
        .expect("changed line should be valid");
    parse_location(LocationRequest {
        commit: fixture.head.to_string(),
        path: "README.md".to_owned(),
        old_start: Some(2),
        old_end: None,
        new_start: None,
        new_end: None,
    })
    .expect_err("incomplete ranges should fail");

    service
        .edit_patch_write(EditPatchRequest {
            subject: subject.clone(),
            title: Some("Improved change".to_owned()),
            revision: Some(revision.clone()),
            description: Some("Improved revision".to_owned()),
        })
        .await
        .expect("patch should be edited");
    assert!(
        service
            .edit_patch_write(EditPatchRequest {
                subject: subject.clone(),
                title: None,
                revision: None,
                description: Some("Missing revision".to_owned()),
            })
            .await
            .is_err()
    );

    service
        .review_write(
            ReviewWriteRequest {
                subject: subject.clone(),
                revision: Some(revision.clone()),
                review: None,
                verdict: None,
                summary: None,
                labels: Vec::new(),
            },
            false,
        )
        .await
        .err()
        .expect("an empty review should be rejected");
    let review = result_id(
        service
            .review_write(
                ReviewWriteRequest {
                    subject: subject.clone(),
                    revision: Some(revision.clone()),
                    review: None,
                    verdict: Some("reject".to_owned()),
                    summary: Some("Needs work".to_owned()),
                    labels: Vec::new(),
                },
                false,
            )
            .await
            .expect("review should be created"),
    );
    let objects = object_count(&repository.backend);
    for (target, entity) in [
        (CommentTargetKind::Revision, revision.clone()),
        (CommentTargetKind::Review, review.clone()),
    ] {
        for _attempt in 0..2 {
            service
                .write_comment(
                    CommentWriteRequest {
                        subject: subject.clone(),
                        target,
                        entity: Some(entity.clone()),
                        comment: None,
                        body: Some("Invalid reply".to_owned()),
                        reply_to: Some("8888888888888888888888888888888888888888".to_owned()),
                        location: None,
                    },
                    CommentOperation::Create,
                )
                .await
                .err()
                .expect("a missing patch reply parent should be rejected");
        }
    }
    assert_eq!(
        object_count(&repository.backend),
        objects,
        "invalid patch replies must not write Git objects"
    );
    let revision_id =
        patch::RevisionId::from(EntryId::from_str(&revision).expect("revision should parse"));
    let ignored_review = {
        let signer = fixture.profile.signer().expect("signer should load");
        let mut patches =
            patch::Cache::no_cache(&repository, &signer).expect("patch cache should open");
        let mut patch = patches.get_mut(&patch_id).expect("patch should load");
        patch
            .transaction("Ignored duplicate review", |tx| {
                tx.review(
                    revision_id,
                    Some(patch::Verdict::Accept),
                    Some("Duplicate".to_owned()),
                    Vec::new(),
                )
            })
            .expect("ignored review operation should be committed")
    };
    assert!(
        !review_mutation_operation(
            &repository,
            patch_id,
            ignored_review,
            false,
            Some(revision_id),
            None,
            Some(patch::Verdict::Accept),
            Some("Duplicate"),
            &[],
        )
        .expect("recovery should be checked"),
        "an ignored review operation must not be recovered as successful"
    );
    for _attempt in 0..2 {
        service
            .review_write(
                ReviewWriteRequest {
                    subject: subject.clone(),
                    revision: Some(revision.clone()),
                    review: None,
                    verdict: Some("accept".to_owned()),
                    summary: Some("Duplicate".to_owned()),
                    labels: Vec::new(),
                },
                false,
            )
            .await
            .err()
            .expect("a duplicate review must not be reported as created");
    }
    service
        .review_write(
            ReviewWriteRequest {
                subject: subject.clone(),
                revision: None,
                review: Some(review.clone()),
                verdict: Some("accept".to_owned()),
                summary: Some("Addressed".to_owned()),
                labels: Vec::new(),
            },
            true,
        )
        .await
        .expect("review should be edited");

    let comment = result_id(
        service
            .write_comment(
                CommentWriteRequest {
                    subject: subject.clone(),
                    target: CommentTargetKind::Review,
                    entity: Some(review.clone()),
                    comment: None,
                    body: Some("Inline finding".to_owned()),
                    reply_to: None,
                    location: Some(location),
                },
                CommentOperation::Create,
            )
            .await
            .expect("review comment should be created"),
    );
    for resolved in [true, false] {
        service
            .resolve_review_comment(ResolveCommentRequest {
                subject: subject.clone(),
                review: review.clone(),
                comment: comment.clone(),
                resolved,
            })
            .await
            .expect("review comment resolution should change");
    }

    service
        .write_comment(
            CommentWriteRequest {
                subject: subject.clone(),
                target: CommentTargetKind::Revision,
                entity: Some(revision.clone()),
                comment: None,
                body: Some("Revision note".to_owned()),
                reply_to: None,
                location: None,
            },
            CommentOperation::Create,
        )
        .await
        .expect("revision comment should be created");
    let legacy_targets = [
        crate::agent::LegacyReplyTarget::Revision {
            revision: radicle::cob::patch::RevisionId::from(
                EntryId::from_str(&revision).expect("revision should parse"),
            ),
            reply_to: None,
        },
        crate::agent::LegacyReplyTarget::Review {
            review: radicle::cob::patch::ReviewId::from(
                EntryId::from_str(&review).expect("review should parse"),
            ),
            reply_to: None,
        },
    ];
    for (action, target) in legacy_targets.into_iter().enumerate() {
        service
            .publisher
            .reconcile_legacy(
                crate::watcher::Subject::Patch(patch_id),
                crate::agent::DeliveryId {
                    operation: EntryId::from_str("6666666666666666666666666666666666666666")
                        .expect("operation should parse"),
                    action,
                },
                *fixture.profile.id(),
                &crate::agent::LegacyReply {
                    body: format!("Legacy patch reply {action}"),
                    target,
                },
            )
            .await
            .expect("legacy patch reply should migrate");
    }
    assert!(
        service
            .merge_patch_write(MergePatchRequest {
                subject: subject.clone(),
                revision: "7777777777777777777777777777777777777777".to_owned(),
                commit: fixture.head.to_string(),
            })
            .await
            .is_err(),
        "an unavailable revision must not be reported as merged"
    );
    assert!(
        service
            .merge_patch_write(MergePatchRequest {
                subject: subject.clone(),
                revision: revision.clone(),
                commit: "8888888888888888888888888888888888888888".to_owned(),
            })
            .await
            .is_err(),
        "an unreachable commit must not be reported as merged"
    );
    service
        .merge_patch_write(MergePatchRequest {
            subject,
            revision,
            commit: fixture.head.to_string(),
        })
        .await
        .expect("patch should merge");
}

#[tokio::test]
#[cfg(not(miri))]
async fn rejects_a_diff_with_a_single_oversized_line() {
    let fixture = RepositoryFixture::new();
    let checkout = git2::Repository::open(&fixture.working).expect("checkout should open");
    let signature =
        git2::Signature::now("MCP Test", "mcp@example.com").expect("signature should be valid");
    fs::write(
        fixture.working.join("README.md"),
        vec![b'x'; MAX_DIFF_BYTES + 1],
    )
    .expect("large line should be written");
    let parent = checkout
        .find_commit(fixture.head)
        .expect("parent should exist");
    let head = commit(&checkout, &signature, "Large line", &[&parent]);
    drop(parent);

    let service = fixture.service();
    let patch = result_id(
        service
            .create_patch_write(
                CreatePatchRequest {
                    title: "Large diff".to_owned(),
                    description: "Exercise response bounds".to_owned(),
                    base: fixture.head.to_string(),
                    head: head.to_string(),
                    labels: Vec::new(),
                    draft: false,
                },
                fixture.working.clone(),
            )
            .await
            .expect("patch should be created"),
    );
    let error = service
        .diff(DiffRequest {
            subject: SubjectRequest {
                kind: SubjectKind::Patch,
                subject: patch,
            },
            revision: None,
        })
        .expect_err("oversized diff should be rejected");

    assert!(matches!(error, ServiceError::DiffLimit));
}

#[tokio::test]
#[cfg(all(not(miri), unix))]
async fn groups_diff_lines_by_raw_non_utf8_paths() {
    let fixture = RepositoryFixture::new();
    let checkout = git2::Repository::open(&fixture.working).expect("checkout should open");
    let parent = checkout
        .find_commit(fixture.head)
        .expect("parent should exist");
    let parent_tree = parent.tree().expect("parent tree should exist");
    let blob = checkout
        .blob(b"first\nsecond\nthird\n")
        .expect("blob should be written");
    let mut tree = checkout
        .treebuilder(Some(&parent_tree))
        .expect("tree builder should open");
    tree.insert(&b"invalid-\xff.txt"[..], blob, 0o100644)
        .expect("raw path should be inserted");
    let tree = tree.write().expect("tree should be written");
    let tree = checkout.find_tree(tree).expect("tree should exist");
    let signature =
        git2::Signature::now("MCP Test", "mcp@example.com").expect("signature should be valid");
    let head = checkout
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Non-UTF-8 path",
            &tree,
            &[&parent],
        )
        .expect("commit should be created");
    drop(parent);
    drop(tree);

    let service = fixture.service();
    let patch = result_id(
        service
            .create_patch_write(
                CreatePatchRequest {
                    title: "Raw path diff".to_owned(),
                    description: "Preserve path grouping".to_owned(),
                    base: fixture.head.to_string(),
                    head: head.to_string(),
                    labels: Vec::new(),
                    draft: false,
                },
                fixture.working.clone(),
            )
            .await
            .expect("patch should be created"),
    );
    let diff = service
        .diff(DiffRequest {
            subject: SubjectRequest {
                kind: SubjectKind::Patch,
                subject: patch,
            },
            revision: None,
        })
        .expect("diff should load");
    let files = diff
        .files
        .iter()
        .filter(|file| {
            file.new_path
                .as_deref()
                .is_some_and(|path| path.contains('\u{fffd}'))
        })
        .collect::<Vec<_>>();

    assert_eq!(files.len(), 1);
    assert!(files[0].lines.len() >= 3);
}

#[test]
fn paginates_timelines_with_stable_operation_cursors() {
    let revision = EntryId::from_str("4444444444444444444444444444444444444444")
        .expect("revision should be valid");
    let matching = |id: &str, revision| {
        Op::new(
            EntryId::from_str(id).expect("operation should be valid"),
            (
                PatchAction::Merge {
                    revision,
                    commit: Oid::from_sha1([3; 20]),
                },
                Vec::new(),
            ),
            PublicKey::from_str(AUTHOR).expect("author should be valid"),
            Timestamp::from_secs(0),
            None,
            Manifest::new(TYPENAME.clone(), Version::default()),
        )
    };
    let first_operation = matching("1111111111111111111111111111111111111111", revision.into());
    let skipped = matching(
        "2222222222222222222222222222222222222222",
        EntryId::from_str("5555555555555555555555555555555555555555")
            .expect("revision should be valid")
            .into(),
    );
    let last_operation = matching("3333333333333333333333333333333333333333", revision.into());
    let operations = vec![first_operation, skipped, last_operation];
    let predicate = |operation: &Op<PatchAction>| {
        revision_action(operation.id, &operation.actions.head, &revision.to_string())
    };
    let page = |operations: Vec<Op<PatchAction>>, cursor| {
        paginate_operations(
            operations
                .into_iter()
                .map(Ok::<_, radicle::cob::stream::error::Ops>),
            cursor,
            Some(1),
            predicate,
        )
        .expect("page should be valid")
    };

    let first = page(operations.clone(), None);
    let cursor = EntryId::from_str(
        first
            .next_cursor
            .as_deref()
            .expect("next page should exist"),
    )
    .expect("cursor should be valid");
    let second = page(operations, Some(cursor));

    assert_eq!(
        first.entries[0]["id"],
        "1111111111111111111111111111111111111111"
    );
    assert_eq!(second.entries.len(), 1);
    assert_eq!(
        second.entries[0]["id"],
        "3333333333333333333333333333333333333333"
    );
    assert_eq!(second.next_cursor, None);
}

#[test]
fn includes_merges_in_revision_history() {
    let revision = EntryId::from_str("1111111111111111111111111111111111111111")
        .expect("revision should be valid");
    let operation = EntryId::from_str("2222222222222222222222222222222222222222")
        .expect("operation should be valid");
    let action = PatchAction::Merge {
        revision: radicle::cob::patch::RevisionId::from(revision),
        commit: Oid::from_sha1([3; 20]),
    };

    assert!(revision_action(operation, &action, &revision.to_string()));
}
