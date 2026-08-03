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
        Embed, ObjectId,
        common::Uri,
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
        CommentIssueRequest, CommentOperation, CommentTargetKind, CommentWriteRequest,
        CreateIssueRequest, CreatePatchRequest, DiffRequest, Document, EditIssueRequest,
        EditPatchRequest, EmbedReadRequest, EmbedWriteRequest, IssueRequest, IssueStateFilter,
        ListIssuesRequest, LocationRequest, MAX_EMBED_BYTES, MergePatchRequest, PreparedEmbed,
        ReactionWriteRequest, ResolveCommentRequest, ReviewCommentRequest, ReviewWriteRequest,
        SecurityIssueRequest, SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind,
        SubjectRequest, mutation_identity, parse_location, parse_object, validate_code_location,
    },
    read::MAX_DIFF_BYTES,
    recovery::{ReactionRecovery, find_review, review_mutation_operation},
    security::SecurityReporting,
    server::Server,
};
#[cfg(not(miri))]
use crate::mcp::server::ServiceError;
use crate::mcp::{
    read::{paginate_operations, revision_action},
    tools::ToolService,
};
#[cfg(not(miri))]
use crate::{
    manifest::QueuePosition, mcp::tools::ToolState, member::Member, queue::WorkQueue,
    watcher::Subject,
};

#[cfg(not(miri))]
const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

#[cfg(not(miri))]
async fn chat_state() -> crate::chat::State {
    crate::chat::State::memory()
        .await
        .expect("chat state should open")
}

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

    fn additional_repository(&self, name: &str) -> RepoId {
        let working = self._root.path().join(name);
        let checkout = git2::Repository::init(&working).expect("repository should initialize");
        checkout
            .set_head("refs/heads/main")
            .expect("main should become HEAD");
        let tree = {
            let oid = checkout
                .treebuilder(None)
                .expect("tree builder should initialize")
                .write()
                .expect("tree should be written");
            checkout.find_tree(oid).expect("tree should be found")
        };
        let signature =
            git2::Signature::now("MCP Test", "mcp@example.com").expect("signature should be valid");
        checkout
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Initial",
                &tree,
                &[],
            )
            .expect("commit should be created");
        let signer = self.profile.signer().expect("signer should load");
        let (repository, _document, _refs) = radicle::rad::init(
            &checkout,
            name.try_into().expect("project name should be valid"),
            "Additional MCP fixture",
            "main".try_into().expect("branch should be valid"),
            Visibility::private([]),
            &signer,
            &self.profile.storage,
        )
        .expect("Radicle repository should initialize");

        repository
    }

    async fn queue(&self) -> WorkQueue {
        WorkQueue::memory(self.repository, Member::Nico)
            .await
            .expect("queue should open")
    }

    async fn service(&self) -> ToolService {
        let publisher = crate::agent::Publisher::new(
            self.profile.clone(),
            self.repository,
            None,
            TestAnnouncer,
            TestAuthorizer(self.repository),
        );
        ToolService::new(
            self.profile.storage.clone(),
            self.repository,
            super::WorkScope::default(),
            publisher,
            crate::manifest::McpAllowlist::default(),
            super::Role::Talker,
            ToolState::new(self.queue().await, chat_state().await),
        )
    }

    async fn journaled_service(
        &self,
        scope: super::WorkScope,
        announcer: impl crate::agent::Announcer,
    ) -> ToolService {
        let publisher = crate::agent::Publisher::new(
            self.profile.clone(),
            self.repository,
            None,
            announcer,
            TestAuthorizer(self.repository),
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
            super::Role::Talker,
            ToolState::new(self.queue().await, chat_state().await),
        )
    }

    async fn service_for(&self, role: super::Role, queue: WorkQueue) -> ToolService {
        let publisher = crate::agent::Publisher::new(
            self.profile.clone(),
            self.repository,
            None,
            TestAnnouncer,
            TestAuthorizer(self.repository),
        );
        ToolService::new(
            self.profile.storage.clone(),
            self.repository,
            super::WorkScope::new(publisher.clone()),
            publisher,
            crate::manifest::McpAllowlist::everything(),
            role,
            ToolState::new(queue, chat_state().await),
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
#[cfg(not(miri))]
fn rejects_duplicate_embed_names() {
    PreparedEmbed::prepare(vec![
        EmbedWriteRequest {
            name: "preview.png".to_owned(),
            content_base64: "Zmlyc3Q=".to_owned(),
        },
        EmbedWriteRequest {
            name: "preview.png".to_owned(),
            content_base64: "c2Vjb25k".to_owned(),
        },
    ])
    .expect_err("duplicate names should be rejected");
}

#[test]
#[cfg(not(miri))]
fn rejects_oversized_embed_before_base64_decoding() {
    let error = PreparedEmbed::prepare(vec![EmbedWriteRequest {
        name: "oversized.bin".to_owned(),
        content_base64: "!".repeat(14_000_000),
    }])
    .expect_err("oversized content should be rejected");

    assert!(error.to_string().contains("exceeds"));
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
        TestAuthorizer(repository),
    );
    let server = Server::start(
        profile.storage.clone(),
        repository,
        publisher,
        crate::manifest::McpAllowlist::default(),
        super::Role::Talker,
        WorkQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open"),
        chat_state().await,
    )
    .await
    .expect("MCP server should start");

    assert!(
        server
            .endpoint()
            .url()
            .host()
            .is_some_and(|host| match host {
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
                url::Host::Domain(_) => false,
            })
    );
    assert!(server.endpoint().url().path().starts_with("/mcp/"));
    server.stop().await.expect("MCP server should stop");
}

#[tokio::test]
#[cfg(not(miri))]
async fn limits_tools_to_the_active_delivery_subject() {
    let scope = super::WorkScope::default();
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
            workspace: Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-delivery"),
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

#[tokio::test]
#[cfg(not(miri))]
async fn authenticated_chat_can_address_the_active_repository() {
    let scope = super::WorkScope::default();
    let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
    scope
        .activate(crate::agent::ChatContext::authenticated(
            repository,
            "general".parse().expect("channel should be valid"),
            crate::chat::MessageId::new(1).expect("message should be valid"),
            crate::chat::AuthenticatedSender::for_test(radicle::identity::Did::from(
                PublicKey::from_str(AUTHOR).expect("author should be valid"),
            )),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-chat"),
        ))
        .await
        .expect("chat message should activate");

    for kind in [SubjectKind::Issue, SubjectKind::Patch] {
        let access = scope
            .access(&SubjectRequest {
                kind,
                subject: "1111111111111111111111111111111111111111".to_owned(),
            })
            .await
            .expect("authenticated chat may address any repository subject");
        assert_eq!(access.context().repository, repository);
    }
}

#[tokio::test]
#[cfg(not(miri))]
async fn chat_reply_targets_only_the_active_channel_and_message() {
    let fixture = RepositoryFixture::new();
    let chat = chat_state().await;
    let channel = "general"
        .parse::<crate::chat::ChannelId>()
        .expect("channel should be valid");
    let message = chat
        .record(
            fixture.repository,
            "team",
            &channel,
            crate::chat::Receipt::Live {
                message: Some("irc-1"),
            },
            "alice",
            fixture.profile.did(),
        )
        .await
        .expect("inbound message should be recorded")
        .id();
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        TestAnnouncer,
        TestAuthorizer(fixture.repository),
    );
    let scope = super::WorkScope::new(publisher.clone());
    let service = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        scope.clone(),
        publisher,
        crate::manifest::McpAllowlist::everything(),
        super::Role::Talker,
        ToolState::new(fixture.queue().await, chat.clone()),
    );
    scope
        .activate(crate::agent::ChatContext::authenticated(
            fixture.repository,
            channel.clone(),
            message,
            crate::chat::AuthenticatedSender::for_test(fixture.profile.did()),
            fixture.working.clone(),
        ))
        .await
        .expect("chat message should activate");

    let reply = service
        .enqueue_chat_reply("On it".to_owned())
        .await
        .expect("reply should enter the outbox");
    let pending = chat
        .pending_replies(fixture.repository)
        .await
        .expect("outbox should be readable");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.get(), reply.reply);
    assert_eq!(pending[0].channel, channel);
    assert_eq!(pending[0].in_reply_to, message);
    assert_eq!(pending[0].body, "On it");

    scope.deactivate().await;
    scope
        .activate(crate::agent::Context {
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
        })
        .await
        .expect("repository delivery should activate");
    service
        .enqueue_chat_reply("Wrong ingress".to_owned())
        .await
        .expect_err("repository activity must not gain a chat target");
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
struct TestAuthorizer(RepoId);

#[cfg(not(miri))]
#[async_trait::async_trait]
impl crate::agent::Authorizer for TestAuthorizer {
    async fn authorize(&self, _method: &str) -> Result<(), crate::agent::PublishError> {
        Ok(())
    }

    async fn security_policy(
        &self,
    ) -> Result<crate::manifest::SecurityPolicy, crate::agent::PublishError> {
        Ok(crate::manifest::SecurityPolicy::test(self.0))
    }
}

#[cfg(not(miri))]
#[derive(Debug, Clone)]
struct RotatingAuthorizer(Arc<tokio::sync::RwLock<crate::manifest::SecurityPolicy>>);

#[cfg(not(miri))]
#[async_trait::async_trait]
impl crate::agent::Authorizer for RotatingAuthorizer {
    async fn authorize(&self, _method: &str) -> Result<(), crate::agent::PublishError> {
        Ok(())
    }

    async fn security_policy(
        &self,
    ) -> Result<crate::manifest::SecurityPolicy, crate::agent::PublishError> {
        Ok(self.0.read().await.clone())
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
    let scope = super::WorkScope::default();
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
        repository: fixture.repository,
        work: context.delivery.into(),
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
    let delivery_service = service.for_work(&access);
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
        .reconcile(context.delivery.into())
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
        .for_work(&access)
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
async fn reconciles_security_writes_from_the_delivery_journal() {
    let fixture = RepositoryFixture::new();
    let security_repository = fixture.additional_repository("security-recovery");
    let announcements = Arc::new(AtomicUsize::new(0));
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        FlakyAnnouncer(Arc::clone(&announcements)),
        TestAuthorizer(security_repository),
    )
    .with_memory_journal()
    .await
    .expect("mutation journal should initialize");
    let scope = super::WorkScope::new(publisher.clone());
    let reporting = SecurityReporting::new(&publisher);
    let service = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        scope.clone(),
        publisher,
        crate::manifest::McpAllowlist::default(),
        super::Role::Talker,
        ToolState::new(fixture.queue().await, chat_state().await),
    );
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
    scope
        .activate(context.clone())
        .await
        .expect("delivery should activate");
    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    let delivery_service = service.for_work(&access);

    let _announcement = reporting
        .report(
            &delivery_service,
            SecurityIssueRequest {
                title: "Interrupted report".to_owned(),
                description: "Announce this once".to_owned(),
            },
        )
        .await
        .expect_err("the first announcement should fail");
    drop(access);
    scope
        .reconcile(context.delivery.into())
        .await
        .expect("security write should reconcile");

    let repository = fixture
        .profile
        .storage
        .repository(security_repository)
        .expect("security repository should open");
    assert_eq!(
        issue::Issues::open(&repository, radicle::cob::store::access::ReadOnly)
            .expect("issues should open")
            .count()
            .expect("issues should count"),
        1
    );
    assert_eq!(announcements.load(Ordering::Relaxed), 2);
}

#[tokio::test]
#[cfg(not(miri))]
async fn recovers_an_unsigned_local_write_without_duplication() {
    let fixture = RepositoryFixture::new();
    let scope = super::WorkScope::default();
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
        repository: fixture.repository,
        work: context.delivery.into(),
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
                let identity = repository.identity_head()?;
                let mut issues = radicle::cob::store::Store::<issue::Issue, _, _>::open(
                    repository,
                    radicle::cob::store::access::WriteAs::new(signer),
                )?
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
                )?;
                Err(crate::agent::PublishError::Rejected(
                    "simulated crash before signing".to_owned(),
                ))
            },
            move |repository, _author, operation, _validation| {
                Issues::open(repository, ReadOnly)?
                    .get(&ObjectId::from(operation))
                    .map(|issue| issue.is_some())
                    .map_err(crate::agent::PublishError::from)
            },
        )
        .await;
    interrupted.expect_err("the simulated crash should interrupt publication");

    let access = scope
        .access_current()
        .await
        .expect("delivery should be accessible");
    service
        .for_work(&access)
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
    let scope = super::WorkScope::default();
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
    let delivery_service = service.for_work(&access);

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
        embeds: vec![EmbedWriteRequest {
            name: "recovery.txt".to_owned(),
            content_base64: "cmVjb3Zlcnk=".to_owned(),
        }],
    };
    delivery_service
        .write_comment(comment_request(), CommentOperation::Create)
        .await
        .expect("comment should be created");
    service
        .publisher
        .forget_completion(&crate::agent::MutationKey {
            repository: fixture.repository,
            work: delivery.into(),
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
    let delivery_service = service.for_work(&access);
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
        embeds: Vec::new(),
        comments: Vec::new(),
    };
    delivery_service
        .review_write(review_request(), false)
        .await
        .expect("review should be created");
    service
        .publisher
        .forget_completion(&crate::agent::MutationKey {
            repository: fixture.repository,
            work: delivery.into(),
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
    let delivery_service = service.for_work(&access);
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
    let scope = super::WorkScope::default();
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
    let delivery_service = service.for_work(&access);
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
            repository: fixture.repository,
            work: delivery.into(),
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
    let delivery_service = service.for_work(&access);
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
    let scope = super::WorkScope::default();
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
    let delivery_service = service.for_work(&access);
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
    let replay = service.for_work(&access);
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
    let service = fixture.service().await;

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
async fn filters_listed_issues_by_state_and_query() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
    let matching = result_id(
        service
            .create_issue_write(CreateIssueRequest {
                title: "Parser crash".to_owned(),
                description: "Crashes on empty input".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("matching issue should be created"),
    );
    let closed = result_id(
        service
            .create_issue_write(CreateIssueRequest {
                title: "Parser rewrite".to_owned(),
                description: "Long-term cleanup".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("closed issue should be created"),
    );
    service
        .set_state_write(
            SetStateRequest {
                subject: RepositoryFixture::subject(SubjectKind::Issue, &closed),
                state: "closed".to_owned(),
            },
            SubjectKind::Issue,
        )
        .await
        .expect("issue state should update");

    let listed = |query: Option<&str>, state| {
        let page = service
            .issues(ListIssuesRequest {
                query: query.map(ToOwned::to_owned),
                state,
                cursor: None,
                limit: None,
            })
            .expect("issues should be listed");
        page.issues
            .into_iter()
            .map(|issue| issue.subject)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        listed(Some("parser"), None).len(),
        2,
        "both issues share the queried word"
    );
    assert_eq!(
        listed(None, Some(IssueStateFilter::Open)),
        std::slice::from_ref(&matching),
        "the closed issue must be excluded by state"
    );
    assert_eq!(
        listed(None, Some(IssueStateFilter::Closed)),
        [closed],
        "the open issue must be excluded by state"
    );
    assert_eq!(
        listed(Some("empty input"), None),
        [matching],
        "the description must be searched and non-matches excluded"
    );
    assert!(
        listed(Some("no-such-issue"), None).is_empty(),
        "a query matching nothing must return nothing"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn manages_issues_and_their_comments() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
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

    let page = service
        .issues(ListIssuesRequest {
            query: Some("bug".to_owned()),
            state: Some(IssueStateFilter::Open),
            cursor: None,
            limit: Some(1),
        })
        .expect("issues should be listed");
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].subject, issue);
    assert_eq!(
        service
            .issue(IssueRequest {
                issue: issue.clone(),
            })
            .expect("issue should load")["title"],
        "Bug"
    );
    service
        .comment_issue_write(CommentIssueRequest {
            issue: issue.clone(),
            body: "Repository-wide finding".to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        })
        .await
        .expect("repository-wide issue comment should be created");

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
                    embeds: Vec::new(),
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
                    embeds: Vec::new(),
                },
                CommentOperation::Create,
            )
            .await
            .expect("comment should be created"),
    );
    let attachment = result_id(
        service
            .write_comment(
                CommentWriteRequest {
                    subject: subject.clone(),
                    target: CommentTargetKind::Issue,
                    entity: None,
                    comment: None,
                    body: Some("Design".to_owned()),
                    reply_to: None,
                    location: None,
                    embeds: vec![EmbedWriteRequest {
                        name: "mockup.txt".to_owned(),
                        content_base64: "aGVsbG8=".to_owned(),
                    }],
                },
                CommentOperation::Create,
            )
            .await
            .expect("comment embed should be created"),
    );
    let issue_id = ObjectId::from_str(&issue).expect("issue should parse");
    let issue_state = Issues::open(&repository, ReadOnly)
        .expect("issues should open")
        .get(&issue_id)
        .expect("issue should load")
        .expect("issue should exist");
    let embed = issue_state
        .thread()
        .comment(&EntryId::from_str(&attachment).expect("comment should parse"))
        .expect("comment should exist")
        .embeds()
        .first()
        .expect("embed should exist");
    assert_eq!(embed.name, "mockup.txt");
    assert_eq!(
        service
            .embed(EmbedReadRequest {
                subject: subject.clone(),
                content: embed.content.to_string(),
            })
            .expect("embed should load")
            .content_base64,
        "aGVsbG8="
    );
    drop(issue_state);
    let oversized = repository
        .backend
        .blob(&vec![0; MAX_EMBED_BYTES + 1])
        .expect("oversized blob should be stored");
    let oversized = Uri::from(radicle::git::Oid::from(oversized));
    {
        let signer = fixture.profile.signer().expect("signer should load");
        let mut issues =
            issue::Cache::no_cache(&repository, &signer).expect("issue cache should open");
        let mut issue = issues.get_mut(&issue_id).expect("issue should load");
        issue
            .transaction("External oversized embed", |tx| {
                tx.comment(
                    "Oversized",
                    *issue_id,
                    vec![Embed {
                        name: "oversized.bin".to_owned(),
                        content: oversized.clone(),
                    }],
                )
            })
            .expect("external embed should be committed");
    }
    assert!(matches!(
        service.embed(EmbedReadRequest {
            subject: subject.clone(),
            content: oversized.to_string(),
        }),
        Err(ServiceError::EmbedLimit)
    ));
    for (comment, active) in [
        (None, true),
        (Some(attachment.clone()), true),
        (Some(attachment), false),
    ] {
        service
            .reaction_write(ReactionWriteRequest {
                subject: subject.clone(),
                target: CommentTargetKind::Issue,
                entity: None,
                comment,
                reaction: "👍".to_owned(),
                active,
                location: None,
            })
            .await
            .expect("issue reaction should update");
    }
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
                embeds: Vec::new(),
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
                embeds: Vec::new(),
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
                        embeds: Vec::new(),
                    },
                    operation,
                )
                .await
                .is_err(),
            "redacted comments must not be mutated"
        );
    }
    for active in [true, false] {
        assert!(
            !ReactionRecovery {
                kind: SubjectKind::Issue,
                target: CommentTargetKind::Issue,
                subject: issue_id,
                entity: None,
                comment: Some(EntryId::from_str(&comment).expect("comment should parse")),
                reaction: "👍".parse().expect("reaction should parse"),
                location: None,
                active,
            }
            .applied(&repository, *fixture.profile.id())
            .expect("reaction state should load"),
            "a missing target must not validate a fresh reaction"
        );
    }
}

#[tokio::test]
#[cfg(not(miri))]
async fn reports_security_issues_independently_of_the_issue_allowlist() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
    let report = SecurityReporting::new(&service.publisher)
        .report(
            &service,
            SecurityIssueRequest {
                title: "Credential exposure".to_owned(),
                description: "A token is written to logs.".to_owned(),
            },
        )
        .await
        .expect("security issue should be reported");

    assert_eq!(report.repository, fixture.repository.to_string());
    assert!(!report.issue.is_empty());
    let issue = service
        .issue(IssueRequest {
            issue: report.issue,
        })
        .expect("encrypted issue should be readable in the fixture");
    let description = issue
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("security issue should have a description");
    assert!(description.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!description.contains("token"));
}

#[tokio::test]
#[cfg(not(miri))]
async fn reloads_the_security_policy_before_reporting() {
    let fixture = RepositoryFixture::new();
    let old_identity = age::x25519::Identity::generate();
    let current_identity = age::x25519::Identity::generate();
    let policy = Arc::new(tokio::sync::RwLock::new(
        crate::manifest::SecurityPolicy::test_with_recipients(
            fixture.repository,
            vec![old_identity.to_public().to_string()],
        ),
    ));
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        TestAnnouncer,
        RotatingAuthorizer(Arc::clone(&policy)),
    );
    let reporting = SecurityReporting::new(&publisher);
    let service = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        super::WorkScope::default(),
        publisher,
        crate::manifest::McpAllowlist::default(),
        super::Role::Talker,
        ToolState::new(fixture.queue().await, chat_state().await),
    );
    *policy.write().await = crate::manifest::SecurityPolicy::test_with_recipients(
        fixture.repository,
        vec![current_identity.to_public().to_string()],
    );

    let report = reporting
        .report(
            &service,
            SecurityIssueRequest {
                title: "Rotated policy".to_owned(),
                description: "Current recipient only".to_owned(),
            },
        )
        .await
        .expect("security issue should be reported");
    let issue = service
        .issue(IssueRequest {
            issue: report.issue,
        })
        .expect("security issue should be readable");
    let description = issue
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("security issue should have a description");

    let _removed = age::decrypt(&old_identity, description.as_bytes())
        .expect_err("removed recipient must not decrypt the report");
    let plaintext = age::decrypt(&current_identity, description.as_bytes())
        .expect("current recipient should decrypt the report");
    let plaintext: SecurityIssueRequest =
        serde_json::from_slice(&plaintext).expect("report should contain its request");
    assert_eq!(plaintext.description, "Current recipient only");
}

#[tokio::test]
#[cfg(not(miri))]
async fn manages_patch_reviews_and_inline_comments() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
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
                embeds: Vec::new(),
                comments: Vec::new(),
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
                    embeds: Vec::new(),
                    comments: vec![
                        ReviewCommentRequest {
                            body: "This allocation is avoidable.".to_owned(),
                            location: Some(LocationRequest {
                                commit: fixture.head.to_string(),
                                path: "file".to_owned(),
                                old_start: None,
                                old_end: None,
                                new_start: Some(1),
                                new_end: Some(2),
                            }),
                        },
                        ReviewCommentRequest {
                            body: "And this name reads backwards.".to_owned(),
                            location: None,
                        },
                    ],
                },
                false,
            )
            .await
            .expect("review should be created"),
    );

    {
        let published = Patches::open(&repository, ReadOnly)
            .expect("patches should open")
            .get(&patch_id)
            .expect("patch should load")
            .expect("patch should exist");
        let review_id = patch::ReviewId::from(EntryId::from_str(&review).expect("id should parse"));
        let comments = find_review(&published, review_id)
            .expect("the review should exist")
            .comments()
            .count();
        assert_eq!(
            comments, 2,
            "every line comment sent with a review must be published with it, so that a reviewer \
             never has to spend several turns saying one thing"
        );
    }
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
                        embeds: Vec::new(),
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
                    embeds: Vec::new(),
                    comments: Vec::new(),
                },
                false,
            )
            .await
            .err()
            .expect("a duplicate review must not be reported as created");
    }
    let review_edit = result_id(
        service
            .review_write(
                ReviewWriteRequest {
                    subject: subject.clone(),
                    revision: None,
                    review: Some(review.clone()),
                    verdict: Some("accept".to_owned()),
                    summary: Some("Addressed".to_owned()),
                    labels: Vec::new(),
                    embeds: vec![EmbedWriteRequest {
                        name: "review.png".to_owned(),
                        content_base64: "cHJldmlldw==".to_owned(),
                    }],
                    comments: Vec::new(),
                },
                true,
            )
            .await
            .expect("review should be edited"),
    );
    let review_commit = repository
        .backend
        .find_commit(
            EntryId::from_str(&review_edit)
                .expect("review edit should parse")
                .into(),
        )
        .expect("review edit commit should exist");
    let review_tree = review_commit.tree().expect("review edit tree should load");
    let review_blob = review_tree
        .get_path(Path::new("embeds/review.png"))
        .expect("review embed should be reachable");
    assert_eq!(
        repository
            .backend
            .find_blob(review_blob.id())
            .expect("review embed blob should exist")
            .content(),
        b"preview"
    );
    let patch_state = Patches::open(&repository, ReadOnly)
        .expect("patches should open")
        .get(&patch_id)
        .expect("patch should load")
        .expect("patch should exist");
    let review_id = patch::ReviewId::from(EntryId::from_str(&review).expect("review should parse"));
    let review_embed = find_review(&patch_state, review_id)
        .expect("review should exist")
        .embeds()
        .first()
        .expect("review embed should exist");
    assert_eq!(review_embed.name, "review.png");
    assert_eq!(
        service
            .embed(EmbedReadRequest {
                subject: subject.clone(),
                content: review_embed.content.to_string(),
            })
            .expect("review embed should load")
            .content_base64,
        "cHJldmlldw=="
    );

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
                embeds: Vec::new(),
            },
            CommentOperation::Create,
        )
        .await
        .err()
        .expect(
            "a review comment posted on its own must be refused, or the batch that keeps a review \
             to one act would be optional",
        );
    let comment = {
        let published = Patches::open(&repository, ReadOnly)
            .expect("patches should open")
            .get(&patch_id)
            .expect("patch should load")
            .expect("patch should exist");
        let review_id = patch::ReviewId::from(EntryId::from_str(&review).expect("id should parse"));
        find_review(&published, review_id)
            .expect("the review should exist")
            .comments()
            .map(|(id, _comment)| id.to_string())
            .next()
            .expect("the review should carry the comments sent with it")
    };

    let _reply =
        result_id(
            service
                .write_comment(
                    CommentWriteRequest {
                        subject: subject.clone(),
                        target: CommentTargetKind::Review,
                        entity: Some(review.clone()),
                        comment: None,
                        body: Some("Answering that line.".to_owned()),
                        reply_to: Some(comment.clone()),
                        location: None,
                        embeds: Vec::new(),
                    },
                    CommentOperation::Create,
                )
                .await
                .expect(
                    "answering somebody else's line comment is a conversation, not a second \
                     review,                  so it must stay possible",
                ),
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

    let revision_comment = result_id(
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
                    embeds: Vec::new(),
                },
                CommentOperation::Create,
            )
            .await
            .expect("revision comment should be created"),
    );
    for (target, entity, comment) in [
        (CommentTargetKind::Revision, revision.clone(), None),
        (
            CommentTargetKind::Revision,
            revision.clone(),
            Some(revision_comment),
        ),
        (CommentTargetKind::Review, review.clone(), None),
        (
            CommentTargetKind::Review,
            review.clone(),
            Some(comment.clone()),
        ),
    ] {
        service
            .reaction_write(ReactionWriteRequest {
                subject: subject.clone(),
                target,
                entity: Some(entity),
                comment,
                reaction: "🚀".to_owned(),
                active: true,
                location: None,
            })
            .await
            .expect("patch reaction should update");
    }
    service
        .reaction_write(ReactionWriteRequest {
            subject: subject.clone(),
            target: CommentTargetKind::Review,
            entity: Some(review.clone()),
            comment: Some(comment),
            reaction: "🚀".to_owned(),
            active: false,
            location: None,
        })
        .await
        .expect("patch reaction should be removed");
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

    let service = fixture.service().await;
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

    let service = fixture.service().await;
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

#[tokio::test]
#[cfg(not(miri))]
async fn each_half_offers_only_the_tools_its_side_can_use() {
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let talker = fixture
        .service_for(super::Role::Talker, queue.clone())
        .await;
    let coder = fixture.service_for(super::Role::Coder, queue).await;

    for tool in ["add_job", "cancel", "note", "work_status"] {
        assert!(
            talker.offers(tool),
            "the talking half records work, so '{tool}' must reach it"
        );
        assert!(
            !coder.offers(tool),
            "'{tool}' would let the building half hand itself work"
        );
    }
    for tool in ["create_patch", "update_patch"] {
        assert!(
            coder.offers(tool),
            "the building half makes the commits '{tool}' needs"
        );
        assert!(
            !talker.offers(tool),
            "the talking half has no commits to turn into a patch with '{tool}'"
        );
    }
    assert!(talker.offers("reply_chat"));
    assert!(!coder.offers("reply_chat"));
    for tool in ["get_subject", "create_comment", "merge_patch"] {
        assert!(talker.offers(tool));
        assert!(
            coder.offers(tool),
            "'{tool}' belongs to whatever the manifest allows, not to one half"
        );
    }
}

#[tokio::test]
#[cfg(not(miri))]
async fn carries_a_note_on_the_next_result_the_building_half_receives() {
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let issue = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("subject should be valid"),
    );
    let job = queue
        .push("write it".to_owned(), issue.into(), QueuePosition::End)
        .await
        .expect("job should be recorded");
    let taken = queue.next().await.expect("queue should be readable");
    assert_eq!(taken.id, job);

    let talker = fixture
        .service_for(super::Role::Talker, queue.clone())
        .await;
    let coder = fixture.service_for(super::Role::Coder, queue.clone()).await;
    queue.note("use the existing parser".to_owned()).await;

    assert!(
        talker.pending_notes().await.is_empty(),
        "a note must not travel back to the conversation that wrote it"
    );
    assert_eq!(
        coder.pending_notes().await,
        ["use the existing parser"],
        "the building half has no inbox, so a note has to arrive on a tool result"
    );
    assert!(
        coder.pending_notes().await.is_empty(),
        "a note already carried must not repeat on every later call"
    );
}
