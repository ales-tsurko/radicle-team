//! MCP integration and repository-service tests.

use std::{
    fs,
    path::Path,
    str::FromStr as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use radicle::{
    cob::{
        Embed, EntryId, Manifest, ObjectId, Op, Timestamp, Version,
        common::Uri,
        issue::{self, Issues},
        patch::{self, Action as PatchAction, Patches, TYPENAME},
        store::access::ReadOnly,
    },
    crypto::{PublicKey, Seed},
    git::Oid,
    identity::{RepoId, doc::Visibility},
    node::Alias,
    profile::{Home, Profile},
    storage::{ReadRepository as _, ReadStorage as _},
};

use crate::{
    agent::{PublishError, Role},
    manifest::QueuePosition,
    mcp::{
        protocol::{
            CommentIssueRequest, CommentOperation, CommentTargetKind, CommentWriteRequest,
            CreateIssueRequest, CreatePatchRequest, DiffRequest, Document, EditIssueRequest,
            EditPatchRequest, EmbedReadRequest, EmbedWriteRequest, IssueRequest, IssueStateFilter,
            ListIssuesRequest, LocationRequest, MAX_EMBED_BYTES, MergePatchRequest, PreparedEmbed,
            ReactionWriteRequest, ResolveCommentRequest, ReviewCommentRequest, ReviewWriteRequest,
            SecurityIssueRequest, SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind,
            SubjectRequest, mutation_identity, parse_location, parse_object,
            validate_code_location,
        },
        read::{MAX_DIFF_BYTES, paginate_operations, revision_action},
        recovery::{ReactionRecovery, find_review, review_mutation_operation},
        security::SecurityReporting,
        server::{Server, ServiceError},
        tools::{ToolService, ToolState},
    },
    member::Member,
    queue::TaskQueue,
    watcher::Subject,
};

const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

async fn chat_state() -> crate::chat::State {
    crate::chat::State::memory()
        .await
        .expect("chat state should open")
}

struct RepositoryFixture {
    _root: tempfile::TempDir,
    profile: Profile,
    repository: RepoId,
    working: std::path::PathBuf,
    base: git2::Oid,
    head: git2::Oid,
}

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

    fn bind_receipts(&self, did: radicle::identity::Did) {
        use radicle::storage::SignRepository as _;
        let repository = self
            .profile
            .storage
            .repository(self.repository)
            .expect("repository");
        let (_, head) = repository.canonical_head().expect("canonical head");
        let git = &repository.backend;
        let parent = git.find_commit(head.into()).expect("canonical commit");
        let mut tree = git
            .treebuilder(Some(&parent.tree().expect("tree")))
            .expect("tree builder");
        let rid = self.repository;
        let policy = format!(
            r#"version = 1
[security]
repository = "{rid}"
recipients = ["age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa"]
[members.nico]
signing_did = "{did}"
generation = 1
state = "active"
mcp_allow = ["read", "reactions"]
"#
        );
        let blob = git.blob(policy.as_bytes()).expect("manifest blob");
        tree.insert(crate::manifest::FILE_NAME, blob, 0o100644)
            .expect("manifest tree");
        let tree = git
            .find_tree(tree.write().expect("write tree"))
            .expect("tree");
        let signature =
            git2::Signature::now("Receipt Test", "receipt@example.com").expect("signature");
        git.commit(
            Some(&format!(
                "refs/namespaces/{}/refs/heads/main",
                self.profile.id()
            )),
            &signature,
            &signature,
            "Bind receipt identity",
            &tree,
            &[&parent],
        )
        .expect("commit manifest");
        repository
            .sign_refs(&self.profile.signer().expect("signer"))
            .expect("sign canonical refs");
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

    async fn queue(&self) -> TaskQueue {
        TaskQueue::memory(self.repository, Member::Nico)
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
            super::ActivityScope::default(),
            publisher,
            crate::manifest::McpAllowlist::default(),
            Role::Talker,
            ToolState::new(self.queue().await, chat_state().await),
        )
    }

    async fn journaled_service(
        &self,
        scope: super::ActivityScope,
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
            Role::Talker,
            ToolState::new(self.queue().await, chat_state().await),
        )
    }

    // Working copies created for real, so the branch a task works on exists.
    async fn prepared_workspaces(
        &self,
        name: &str,
        queue: TaskQueue,
    ) -> crate::workspace::Workspaces {
        let workspaces = crate::workspace::Workspaces::memory(
            name,
            self.profile.storage.clone(),
            self.repository,
            self.profile.info(),
            queue,
        )
        .expect("working copies should be created");
        workspaces
            .initialize()
            .await
            .expect("working copies should be initialized");

        workspaces
    }

    async fn service_with(&self, role: Role, queue: TaskQueue) -> ToolService {
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
            super::ActivityScope::new(publisher.clone()),
            publisher,
            crate::manifest::McpAllowlist::everything(),
            role,
            ToolState::new(queue, chat_state().await),
        )
    }

    async fn service_as(&self, member: Member, role: Role, queue: TaskQueue) -> ToolService {
        let home = Home::new(self._root.path().join(format!("{}-{role:?}", member.id())))
            .expect("member home should open");
        let mut profile = Profile::init(home, Alias::new(member.id()), None, Seed::generate())
            .expect("member identity should initialize");
        profile.storage = self.profile.storage.clone();
        let publisher = crate::agent::Publisher::new(
            profile,
            self.repository,
            None,
            TestAnnouncer,
            TestAuthorizer(self.repository),
        )
        .with_memory_journal()
        .await
        .expect("journal should open");
        ToolService::new(
            self.profile.storage.clone(),
            self.repository,
            super::ActivityScope::new(publisher.clone()),
            publisher,
            crate::manifest::McpAllowlist::everything(),
            role,
            ToolState::new(queue, chat_state().await),
        )
    }

    async fn activate(
        &self,
        service: &ToolService,
        source: Subject,
        activity: crate::agent::ActivityId,
    ) {
        let delivery = match activity {
            crate::agent::ActivityId::Task(id) => {
                service
                    .queue
                    .get(id)
                    .await
                    .expect("task should exist")
                    .context
                    .activity
            }
            _ => None,
        };
        service
            .scope
            .activate(super::ActivityContext {
                repository: self.repository,
                source: source.into(),
                activity,
                workspace: self.working.clone(),
                delivery,
            })
            .await
            .expect("work should activate");
    }

    async fn deliver(&self, service: &ToolService, activity: crate::watcher::Activity) {
        service.scope.deactivate().await;
        service
            .scope
            .activate(super::ActivityContext::delivery(
                activity,
                self.working.clone(),
            ))
            .await
            .expect("delivery should activate");
    }

    fn activity(
        &self,
        subject: Subject,
        operation: EntryId,
        kind: crate::watcher::Kind,
    ) -> crate::watcher::Activity {
        crate::watcher::Activity {
            repository: self.repository,
            subject,
            operation,
            action: 0,
            author: self.profile.did(),
            kind,
        }
    }

    async fn patch(&self, author: &ToolService) -> Subject {
        let id = result_id(
            author
                .create_patch_write(
                    CreatePatchRequest {
                        title: "Change".to_owned(),
                        description: "Focused change".to_owned(),
                        base: self.base.to_string(),
                        head: self.head.to_string(),
                        labels: Vec::new(),
                        draft: false,
                    },
                    self.working.clone(),
                )
                .await
                .expect("patch should publish"),
        );
        Subject::Patch(ObjectId::from_str(&id).expect("patch should parse"))
    }

    async fn publish_receipts(
        &self,
        inbox: &crate::delivery::Inbox,
        publisher: &crate::agent::Publisher,
    ) {
        let publish = inbox.publish(publisher, Member::Nico, Member::Nico.receipt());
        let published = async {
            while !inbox.pending().await.expect("pending receipts").is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = publish => panic!("receipt publisher should stay running: {result:?}"),
                () = published => {},
            }
        })
        .await
        .expect("receipts should publish");
    }

    fn subject(kind: SubjectKind, id: impl ToString) -> SubjectRequest {
        SubjectRequest {
            kind,
            subject: id.to_string(),
        }
    }
}

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

fn result_id(result: rmcp::Json<Document>) -> String {
    result
        .0
        .data
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("write result should contain an id")
        .to_owned()
}

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
        Role::Talker,
        ToolState::new(
            TaskQueue::memory(repository, Member::Nico)
                .await
                .expect("queue should open"),
            chat_state().await,
        ),
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
async fn limits_tools_to_the_active_delivery_subject() {
    let scope = super::ActivityScope::default();
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
    let access = scope.access_current().await.expect("work should be active");
    access
        .require_subject(&SubjectRequest {
            kind: SubjectKind::Issue,
            subject: subject.to_string(),
        })
        .await
        .expect("active issue should be accessible");
    let _wrong_subject = access
        .require_subject(&SubjectRequest {
            kind: SubjectKind::Patch,
            subject: subject.to_string(),
        })
        .await
        .expect_err("different subject kind should be rejected");
    drop(access);
    scope.deactivate().await;
    let _inactive = scope
        .access_current()
        .await
        .expect_err("deactivated scope should reject access");
}

#[tokio::test]
async fn authenticated_chat_can_address_the_active_repository() {
    let scope = super::ActivityScope::default();
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
        let access = scope.access_current().await.expect("chat should be active");
        access
            .require_subject(&SubjectRequest {
                kind,
                subject: "1111111111111111111111111111111111111111".to_owned(),
            })
            .await
            .expect("authenticated chat may address any repository subject");
        assert_eq!(access.context().repository, repository);
    }
}

#[tokio::test]
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
    let scope = super::ActivityScope::new(publisher.clone());
    let service = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        scope.clone(),
        publisher,
        crate::manifest::McpAllowlist::everything(),
        Role::Talker,
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

#[derive(Debug, Clone, Copy)]
struct TestAnnouncer;

impl crate::agent::Announcer for TestAnnouncer {
    fn announce(
        &self,
        _repository: RepoId,
        _namespace: PublicKey,
    ) -> Result<(), crate::agent::PublishError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct TestAuthorizer(RepoId);

#[async_trait::async_trait]
impl crate::agent::Authorizer for TestAuthorizer {
    async fn authorize(&self, method: &str) -> Result<(), crate::agent::PublishError> {
        (method == "delivery_status" || crate::manifest::McpAllowlist::everything().allows(method))
            .then_some(())
            .ok_or_else(|| {
                PublishError::Authorization(format!("MCP method '{method}' is not allowed"))
            })
    }

    async fn security_policy(
        &self,
    ) -> Result<crate::manifest::SecurityPolicy, crate::agent::PublishError> {
        Ok(crate::manifest::SecurityPolicy::test(self.0))
    }
}

#[derive(Debug, Clone)]
struct RotatingAuthorizer(Arc<tokio::sync::RwLock<crate::manifest::SecurityPolicy>>);

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

#[derive(Debug, Clone)]
struct FlakyAnnouncer(Arc<AtomicUsize>);

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
async fn recovers_a_completed_write_when_the_delivery_is_replayed() {
    let fixture = RepositoryFixture::new();
    let scope = super::ActivityScope::default();
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
        activity: context.delivery.into(),
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
    let scope = super::ActivityScope::new(publisher.clone());
    let reporting = SecurityReporting::new(&publisher);
    let service = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        scope.clone(),
        publisher,
        crate::manifest::McpAllowlist::default(),
        Role::Talker,
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
async fn recovers_an_unsigned_local_write_without_duplication() {
    let fixture = RepositoryFixture::new();
    let scope = super::ActivityScope::default();
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
        activity: context.delivery.into(),
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
async fn recovers_created_comments_and_reviews_by_operation_id() {
    let fixture = RepositoryFixture::new();
    let scope = super::ActivityScope::default();
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
            activity: delivery.into(),
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
            activity: delivery.into(),
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
async fn replays_stale_mutations_and_distinguishes_repeated_invocations() {
    let fixture = RepositoryFixture::new();
    let scope = super::ActivityScope::default();
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
            activity: delivery.into(),
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
async fn distinguishes_identical_mutations_and_replays_concurrent_writes() {
    let fixture = RepositoryFixture::new();
    let scope = super::ActivityScope::default();
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
        super::ActivityScope::default(),
        publisher,
        crate::manifest::McpAllowlist::default(),
        Role::Talker,
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
async fn rejects_incomplete_or_incompatible_review_requests_before_publication() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
    let request = || ReviewWriteRequest {
        subject: RepositoryFixture::subject(
            SubjectKind::Patch,
            "1111111111111111111111111111111111111111",
        ),
        revision: Some("2222222222222222222222222222222222222222".to_owned()),
        review: Some("3333333333333333333333333333333333333333".to_owned()),
        verdict: Some("reject".to_owned()),
        summary: Some("Storage errors need handling".to_owned()),
        labels: Vec::new(),
        embeds: Vec::new(),
        comments: Vec::new(),
    };
    for (request, edit, expected) in [
        (
            ReviewWriteRequest {
                review: None,
                ..request()
            },
            true,
            "review is required",
        ),
        (
            ReviewWriteRequest {
                summary: None,
                ..request()
            },
            true,
            "summary is required",
        ),
        (
            ReviewWriteRequest {
                revision: None,
                ..request()
            },
            false,
            "revision is required",
        ),
        (
            ReviewWriteRequest {
                embeds: vec![EmbedWriteRequest {
                    name: "review.png".to_owned(),
                    content_base64: "cHJldmlldw==".to_owned(),
                }],
                ..request()
            },
            false,
            "create_review cannot include embeds",
        ),
        (
            ReviewWriteRequest {
                comments: vec![ReviewCommentRequest {
                    body: "Fix storage errors".to_owned(),
                    location: None,
                }],
                ..request()
            },
            true,
            "edit_review cannot include line comments",
        ),
        (
            ReviewWriteRequest {
                comments: vec![ReviewCommentRequest {
                    body: " \n\t".to_owned(),
                    location: None,
                }],
                ..request()
            },
            false,
            "a review comment body cannot be empty",
        ),
    ] {
        let error = service
            .review_write(request, edit)
            .await
            .err()
            .expect("invalid review requests must fail before trying to publish an operation");
        assert!(
            matches!(error, ServiceError::Request(ref reason) if reason.contains(expected)),
            "{error}"
        );
    }
}

#[tokio::test]
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
                                path: "README.md".to_owned(),
                                old_start: None,
                                old_end: None,
                                new_start: Some(2),
                                new_end: Some(3),
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
        let review = find_review(&published, review_id).expect("the review should exist");
        assert_eq!(
            review.comments().count(),
            2,
            "every line comment sent with a review must be published with it, so that a reviewer \
             never has to spend several turns saying one thing"
        );
        assert_eq!(
            review
                .comments()
                .filter_map(|(_id, comment)| comment.location())
                .collect::<Vec<_>>(),
            vec![&parsed_location],
            "inline findings must retain their diff location"
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
#[cfg(unix)]
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
async fn each_role_offers_only_the_tools_it_can_use() {
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    let chatter = fixture.service_with(Role::Chatter, queue.clone()).await;
    let coder = fixture.service_with(Role::Coder, queue).await;

    for tool in ["add_task", "cancel", "note", "task_status"] {
        assert!(
            talker.offers(tool),
            "the talking role records work, so '{tool}' must reach it"
        );
        assert!(
            chatter.offers(tool),
            "a request made in chat becomes queued work, so '{tool}' must reach it"
        );
        assert!(
            !coder.offers(tool),
            "'{tool}' would let the building role hand itself work"
        );
    }
    for tool in ["create_patch", "update_patch"] {
        assert!(
            coder.offers(tool),
            "the building role makes the commits '{tool}' needs"
        );
        for other in [&talker, &chatter] {
            assert!(
                !other.offers(tool),
                "only the building role has commits to turn into a patch with '{tool}'"
            );
        }
    }
    assert!(
        chatter.offers("reply_chat"),
        "the chatting role is the one holding an authenticated message to answer"
    );
    assert!(!talker.offers("reply_chat"));
    assert!(!coder.offers("reply_chat"));
    for tool in [
        "get_subject",
        "create_comment",
        "merge_patch",
        "get_task_context",
        "update_task_context",
    ] {
        for role in [&talker, &chatter, &coder] {
            assert!(
                role.offers(tool),
                "'{tool}' belongs to whatever the manifest allows, not to one role"
            );
        }
    }
}

#[tokio::test]
async fn carries_a_note_on_the_next_result_the_building_half_receives() {
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let issue = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("subject should be valid"),
    );
    let task = queue
        .push(crate::queue::TaskRequest::new(
            "write it".to_owned(),
            issue.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;
    let taken = queue.next().await.expect("queue should be readable");
    assert_eq!(taken.id, task);

    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    coder
        .scope
        .activate(super::ActivityContext {
            repository: fixture.repository,
            source: issue.into(),
            activity: crate::agent::ActivityId::Task(task),
            workspace: fixture.working.clone(),
            delivery: None,
        })
        .await
        .expect("task should activate");
    queue
        .note(task, "use the existing parser".to_owned())
        .await
        .expect("note should save");

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

// The patch exists the moment this call returns, and the task can still crash afterwards. If the
// queue only learned about it when the task ended, a run picking that task up again would start
// over on the issue it came from and leave the patch behind.
#[tokio::test]
async fn records_the_patch_a_task_produced_before_the_task_can_fail() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::mcp::protocol::UpdatePatchRequest;

    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let issue = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("subject should be valid"),
    );
    let task = queue
        .push(crate::queue::TaskRequest::new(
            "answer it".to_owned(),
            issue.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;
    let taken = queue.next().await.expect("queue should be readable");
    let workspaces = fixture
        .prepared_workspaces("mcp-produced", queue.clone())
        .await;
    workspaces
        .prepare_task(&taken)
        .await
        .expect("the coder should take the issue");
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    coder
        .scope
        .activate(super::ActivityContext {
            repository: fixture.repository,
            source: issue.into(),
            activity: crate::agent::ActivityId::Task(task),
            workspace: fixture.working.clone(),
            delivery: None,
        })
        .await
        .expect("the task should become the active activity");

    let created = coder
        .create_patch(rmcp::handler::server::wrapper::Parameters(
            CreatePatchRequest {
                title: "Answer".to_owned(),
                description: "Patch".to_owned(),
                base: fixture.head.to_string(),
                head: fixture.head.to_string(),
                labels: Vec::new(),
                draft: true,
            },
        ))
        .await
        .expect("patch should be created");
    let patch = ObjectId::from_str(&result_id(created)).expect("patch should parse");
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let patches = Patches::open(&stored, ReadOnly).expect("patches should open");
    let draft = patches
        .get(&patch)
        .expect("draft should read")
        .expect("draft should exist before implementation");
    assert_eq!(draft.state(), &patch::State::Draft);
    assert_eq!(
        draft.base(),
        draft.head(),
        "an early draft needs no fake commit"
    );

    assert_eq!(
        queue.recover().await.expect("queue should be readable"),
        1,
        "the task is still running, so the next start must put it back"
    );
    let recovered = queue.next().await.expect("queue should be readable");

    assert_eq!(
        recovered.source,
        issue.into(),
        "the original request remains available after publication"
    );
    assert_eq!(
        branches(&workspaces),
        [format!("nico-rt/task-{}", taken.id)],
        "the branch is named after the task, so producing a patch leaves it exactly where it was"
    );

    let second = coder
        .create_patch(rmcp::handler::server::wrapper::Parameters(
            CreatePatchRequest {
                title: "Another".to_owned(),
                description: "Patch".to_owned(),
                base: fixture.base.to_string(),
                head: fixture.head.to_string(),
                labels: Vec::new(),
                draft: false,
            },
        ))
        .await;

    let second = second.expect("repeated creation should return the existing patch");
    assert_eq!(result_id(second), patch.to_string());
    coder
        .update_patch(Parameters(UpdatePatchRequest {
            subject: Subject::Patch(patch).into(),
            description: "Implemented and validated".to_owned(),
            base: fixture.base.to_string(),
            head: fixture.head.to_string(),
        }))
        .await
        .expect("implementation should revise the same draft");
    let updated = patches
        .get(&patch)
        .expect("patch should read")
        .expect("patch should exist");
    assert_eq!(updated.state(), &patch::State::Draft);
    assert_eq!(*updated.base(), Oid::from(fixture.base));
    assert_eq!(*updated.head(), Oid::from(fixture.head));
    coder
        .set_state_write(
            SetStateRequest {
                subject: Subject::Patch(patch).into(),
                state: "open".to_owned(),
            },
            SubjectKind::Patch,
        )
        .await
        .expect("the completed draft should become ready");
    assert!(matches!(
        patches
            .get(&patch)
            .expect("patch should read")
            .expect("patch should exist")
            .state(),
        patch::State::Open { .. }
    ));
    assert_eq!(
        queue
            .get(task)
            .await
            .expect("task should remain")
            .context
            .patch(),
        Some(Subject::Patch(patch))
    );
    assert_eq!(
        Patches::open(&stored, ReadOnly)
            .expect("patches should open")
            .all()
            .expect("patches should list")
            .count(),
        1
    );
}

// Returns the branches the runtime made, in order.
fn branches(workspaces: &crate::workspace::Workspaces) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--list", "--format=%(refname:short)", "nico-rt/*"])
        .current_dir(workspaces.store())
        .output()
        .expect("git should run");

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

// Missing task storage must fail before creating an artifact the runtime cannot track.
#[tokio::test]
async fn refuses_to_publish_when_the_task_record_is_unavailable() {
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let issue = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("subject should be valid"),
    );
    let task = queue
        .push(crate::queue::TaskRequest::new(
            "answer it".to_owned(),
            issue.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should be recorded")
        .task;
    let taken = queue.next().await.expect("queue should be readable");
    let workspaces = fixture
        .prepared_workspaces("mcp-unrecorded", queue.clone())
        .await;
    workspaces
        .prepare_task(&taken)
        .await
        .expect("the coder should take the issue");
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    coder
        .scope
        .activate(super::ActivityContext {
            repository: fixture.repository,
            source: issue.into(),
            activity: crate::agent::ActivityId::Task(task),
            workspace: fixture.working.clone(),
            delivery: None,
        })
        .await
        .expect("the task should become the active activity");
    // Taken away underneath the running task, which keeps what it is working on in memory.
    queue
        .database()
        .execute("DROP TABLE task_queue")
        .await
        .expect("the queue's storage should be removable");

    let created = coder
        .create_patch(rmcp::handler::server::wrapper::Parameters(
            CreatePatchRequest {
                title: "Answer".to_owned(),
                description: "Patch".to_owned(),
                base: fixture.base.to_string(),
                head: fixture.head.to_string(),
                labels: Vec::new(),
                draft: false,
            },
        ))
        .await;

    assert!(
        created.is_err(),
        "the task is unavailable, so publication must fail before creating an orphan"
    );
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        Patches::open(&stored, ReadOnly)
            .expect("patches should open")
            .all()
            .expect("patches should list")
            .count(),
        0,
        "unavailable task context must prevent creating an untracked artifact"
    );
    assert_eq!(
        branches(&workspaces),
        [format!("nico-rt/task-{}", taken.id)],
        "and the branch is still the task's, which is the one thing a failed write cannot desync"
    );
}

#[tokio::test]
async fn maya_and_nico_revise_one_patch_and_review_it_from_the_issue() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{agent::ActivityId, mcp::protocol::UpdatePatchRequest, queue::TaskRequest};

    let fixture = RepositoryFixture::new();
    let maya_queue = TaskQueue::memory(fixture.repository, Member::Maya)
        .await
        .expect("queue should open");
    let nico_queue = TaskQueue::memory(fixture.repository, Member::Nico)
        .await
        .expect("queue should open");
    let maya = fixture
        .service_as(Member::Maya, Role::Talker, maya_queue)
        .await;
    let nico = fixture
        .service_as(Member::Nico, Role::Coder, nico_queue.clone())
        .await;
    let operator = fixture.service().await;
    let planning = result_id(
        operator
            .create_issue_write(CreateIssueRequest {
                title: "Plan scaffold".to_owned(),
                description: "Create a focused scaffold task".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("planning issue should create"),
    );
    let planning = Subject::Issue(ObjectId::from_str(&planning).expect("issue should parse"));
    let operation = EntryId::from_str(&planning.id().to_string()).expect("operation should parse");
    fixture
        .activate(
            &maya,
            planning,
            crate::agent::DeliveryId {
                operation,
                action: 0,
            }
            .into(),
        )
        .await;
    let issue_request = || CreateIssueRequest {
        title: "Scaffold CLI".to_owned(),
        description: "Scaffold only; no feature commands".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let issue_id = result_id(
        maya.create_issue(Parameters(issue_request()))
            .await
            .expect("issue should create"),
    );
    assert_eq!(
        result_id(
            maya.create_issue(Parameters(issue_request()))
                .await
                .expect("repeated issue creation should reuse")
        ),
        issue_id
    );
    let issue = Subject::Issue(ObjectId::from_str(&issue_id).expect("issue should parse"));
    assert!(
        maya.access_subject(&SubjectRequest::from(issue))
            .await
            .is_ok(),
        "a newly created issue must be usable immediately"
    );
    let mut request =
        TaskRequest::new("Scaffold only".to_owned(), issue.into(), QueuePosition::End);
    request.request = Some("operation:scaffold".to_owned());
    let task = nico_queue
        .push(request)
        .await
        .expect("task should record")
        .task;
    nico_queue.next().await.expect("task should start");
    fixture.activate(&nico, issue, ActivityId::Task(task)).await;

    let checkout = git2::Repository::open(&fixture.working).expect("checkout should open");
    fs::write(fixture.working.join("README.md"), "# Scaffold\n").expect("scaffold should write");
    let parent = checkout
        .find_commit(fixture.head)
        .expect("parent should exist");
    let signature =
        git2::Signature::now("Nico Test", "nico@example.com").expect("signature should create");
    let head = commit(&checkout, &signature, "Scaffold", &[&parent]);
    let patch_request = || CreatePatchRequest {
        title: "Scaffold".to_owned(),
        description: format!("Implements {issue}"),
        base: fixture.base.to_string(),
        head: head.to_string(),
        labels: Vec::new(),
        draft: false,
    };
    let patch_id = result_id(
        nico.create_patch(Parameters(patch_request()))
            .await
            .expect("patch should publish"),
    );
    assert_eq!(
        result_id(
            nico.create_patch(Parameters(patch_request()))
                .await
                .expect("repeated patch creation should reuse")
        ),
        patch_id
    );
    let patch = Subject::Patch(ObjectId::from_str(&patch_id).expect("patch should parse"));
    nico.scope.deactivate().await;
    fixture.activate(&nico, issue, ActivityId::Task(task)).await;
    let revision = result_id(
        nico.update_patch(Parameters(UpdatePatchRequest {
            subject: SubjectRequest::from(patch),
            description: "Correct base only".to_owned(),
            base: fixture.head.to_string(),
            head: head.to_string(),
        }))
        .await
        .expect("the issue task should revise its recorded patch after reactivation"),
    );
    let invitation = result_id(
        operator
            .comment_issue_write(CommentIssueRequest {
                issue: issue_id.clone(),
                body: format!("@maya please review {patch}"),
                reply_to: None,
                embeds: Vec::new(),
            })
            .await
            .expect("delegate should request the review"),
    );
    fixture
        .deliver(
            &maya,
            fixture.activity(
                issue,
                EntryId::from_str(&invitation).expect("invitation should parse"),
                crate::watcher::Kind::Delivered(crate::watcher::Delivery::Mentioned(
                    crate::watcher::Target::Comment(crate::watcher::Comment::Issue(
                        EntryId::from_str(&invitation).expect("invitation should parse"),
                    )),
                )),
            ),
        )
        .await;
    maya.get_subject(Parameters(SubjectRequest::from(patch)))
        .await
        .expect("Maya should read the patch from the issue");
    maya.create_review(Parameters(ReviewWriteRequest {
        subject: SubjectRequest::from(patch),
        revision: Some(revision.clone()),
        review: None,
        verdict: Some("accept".to_owned()),
        summary: Some("The focused scaffold meets the agreed scope".to_owned()),
        labels: Vec::new(),
        embeds: Vec::new(),
        comments: Vec::new(),
    }))
    .await
    .expect("Maya should publish the review on the patch from this issue task");
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let patches = Patches::open(&stored, ReadOnly).expect("patches should open");
    assert_eq!(patches.all().expect("patches should list").count(), 1);
    let published = patches
        .get(&patch.id())
        .expect("patch should read")
        .expect("patch should exist");
    assert_eq!(published.head().to_string(), head.to_string());
    assert_eq!(published.base().to_string(), fixture.head.to_string());
    assert_eq!(published.author().public_key(), &nico.publisher.author());
    assert!(
        published
            .reviews_of(patch::RevisionId::from(
                EntryId::from_str(&revision).expect("revision should parse")
            ))
            .any(|(_, review)| review.author().public_key() == &maya.publisher.author())
    );
}

#[tokio::test]
async fn saved_supporting_references_do_not_authorize_unrelated_writes() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::ActivityId,
        queue::{Artifact, ArtifactRole, ContextUpdate, TaskRequest},
    };

    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
    let issue = |title: &str| CreateIssueRequest {
        title: title.to_owned(),
        description: "Unrelated task".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let origin = Subject::Issue(
        ObjectId::from_str(&result_id(
            service
                .create_issue_write(issue("Original"))
                .await
                .expect("issue should create"),
        ))
        .expect("issue should parse"),
    );
    let other = Subject::Issue(
        ObjectId::from_str(&result_id(
            service
                .create_issue_write(issue("Other"))
                .await
                .expect("issue should create"),
        ))
        .expect("issue should parse"),
    );
    let queue = fixture.queue().await;
    let task = queue
        .push(TaskRequest::new(
            "work".to_owned(),
            origin.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    queue.next().await.expect("task should start");
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, origin, ActivityId::Task(task))
        .await;
    coder
        .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
            task: None,
            update: ContextUpdate {
                artifacts: vec![Artifact {
                    subject: other.to_string(),
                    role: ArtifactRole::Supporting,
                    purpose: "Useful information".to_owned(),
                }],
                ..ContextUpdate::default()
            },
        }))
        .await
        .expect("supporting reference should save");
    coder
        .get_subject(Parameters(SubjectRequest::from(other)))
        .await
        .expect("repository reads should work");
    assert!(
        coder
            .access_subject(&SubjectRequest::from(other))
            .await
            .is_err(),
        "saved references must not invent task write access"
    );
    let document = coder
        .get_task_context(Parameters(super::protocol::ContextRequest { task: None }))
        .await
        .expect("context should read");
    assert_eq!(document.0.context.artifacts.len(), 2);
}

#[tokio::test]
async fn assignment_and_mention_from_one_operation_record_one_task() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::{ActivityId, DeliveryId},
        queue::TaskRequest,
    };

    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let subject = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("subject should parse"),
    );
    let operation = EntryId::from_str("2222222222222222222222222222222222222222")
        .expect("operation should parse");
    let mut assignment = TaskRequest::new(
        "assigned scaffold".to_owned(),
        subject.into(),
        QueuePosition::End,
    );
    assignment.request = Some(
        ActivityId::from(DeliveryId {
            operation,
            action: 1,
        })
        .request(),
    );
    let first = queue
        .push(assignment)
        .await
        .expect("assignment should record");
    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    fixture
        .activate(
            &talker,
            subject,
            DeliveryId {
                operation,
                action: 0,
            }
            .into(),
        )
        .await;
    let recorded = talker
        .add_task(Parameters(super::protocol::AddTaskRequest {
            kind: None,
            brief: "same scaffold requested by mention".to_owned(),
            position: QueuePosition::End,
            context: crate::queue::ContextUpdate::default(),
            distinct: false,
        }))
        .await
        .expect("mention should reuse assigned work");
    assert_eq!(recorded.0.task, first.task);
    assert_eq!(queue.waiting().await.expect("queue should read"), 1);
}

#[tokio::test]
async fn patch_review_requests_report_their_effective_queue_priority() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{agent::DeliveryId, queue::TaskRequest};

    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let subject = fixture.patch(&fixture.service().await).await;
    let operation = EntryId::from_str("2222222222222222222222222222222222222222")
        .expect("operation should parse");
    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    fixture
        .activate(
            &talker,
            subject,
            DeliveryId {
                operation,
                action: 0,
            }
            .into(),
        )
        .await;
    let request = || super::protocol::AddTaskRequest {
        kind: None,
        brief: "Address the review".to_owned(),
        position: QueuePosition::End,
        context: crate::queue::ContextUpdate::default(),
        distinct: false,
    };
    let recorded = talker
        .add_task(Parameters(request()))
        .await
        .expect("review work should record");
    assert_eq!(recorded.0.position, QueuePosition::Next);
    let mut urgent = TaskRequest::new(
        "Urgent correction".to_owned(),
        subject.into(),
        QueuePosition::Front,
    );
    urgent.request = Some("explicit-urgency".to_owned());
    let promoted = queue.push(urgent).await.expect("work should promote");
    let repeated = talker
        .add_task(Parameters(request()))
        .await
        .expect("repeated request should reuse urgent work");
    assert_eq!(recorded.0.task, promoted.task);
    assert_eq!(repeated.0.task, recorded.0.task);
    assert_eq!(repeated.0.position, QueuePosition::Front);
    assert_eq!(repeated.0.waiting, 1);
}

#[tokio::test]
async fn native_issue_assignment_reports_delegate_requirement_without_creating_an_issue() {
    let fixture = RepositoryFixture::new();
    let queue = TaskQueue::memory(fixture.repository, Member::Maya)
        .await
        .expect("queue should open");
    let maya = fixture.service_as(Member::Maya, Role::Talker, queue).await;
    let failure = maya
        .create_issue_write(CreateIssueRequest {
            title: "Assign scaffold".to_owned(),
            description: "Task".to_owned(),
            labels: Vec::new(),
            assignees: vec![format!("did:key:{AUTHOR}")],
        })
        .await
        .map(|_| ())
        .expect_err("non-delegate assignment should fail before creating an issue");
    assert!(matches!(failure, ServiceError::IssueDelegate(_)));
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        Issues::open(&stored, ReadOnly)
            .expect("issues should open")
            .all()
            .expect("issues should list")
            .count(),
        0
    );
}

#[derive(Debug)]
struct FailedContextWriteAnnouncer {
    database: std::path::PathBuf,
    calls: AtomicUsize,
}

impl crate::agent::Announcer for FailedContextWriteAnnouncer {
    fn announce(&self, _repository: RepoId, _namespace: PublicKey) -> Result<(), PublishError> {
        if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            sqlite::open(&self.database)
                .and_then(|database| {
                    database.execute(
                        "CREATE TRIGGER fail_task_context_write BEFORE INSERT ON task_contexts
                         BEGIN SELECT RAISE(ABORT, 'task context write failed'); END",
                    )
                })
                .map_err(|error| PublishError::Task(error.to_string()))?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn recovers_one_patch_when_recording_its_task_relationship_failed_after_publication() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::{ActivityId, Publisher},
        queue::TaskRequest,
    };

    let fixture = RepositoryFixture::new();
    let path = fixture._root.path().join("queue.db");
    let queue = TaskQueue::open(&path, fixture.repository, Member::Nico)
        .await
        .expect("queue should open");
    let source = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111").expect("issue should parse"),
    );
    let task = queue
        .push(TaskRequest::new(
            "scaffold".to_owned(),
            source.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    queue.next().await.expect("task should start");
    let saved_context = queue.get(task).await.expect("context should read").context;
    let publisher = Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        FailedContextWriteAnnouncer {
            database: path,
            calls: AtomicUsize::new(0),
        },
        TestAuthorizer(fixture.repository),
    )
    .with_memory_journal()
    .await
    .expect("journal should open");
    let coder = ToolService::new(
        fixture.profile.storage.clone(),
        fixture.repository,
        super::ActivityScope::new(publisher.clone()),
        publisher,
        crate::manifest::McpAllowlist::everything(),
        Role::Coder,
        ToolState::new(queue.clone(), chat_state().await),
    );
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    let request = || CreatePatchRequest {
        title: "Scaffold".to_owned(),
        description: "Focused scaffold".to_owned(),
        base: fixture.base.to_string(),
        head: fixture.head.to_string(),
        labels: Vec::new(),
        draft: false,
    };
    coder
        .create_patch(Parameters(request()))
        .await
        .map(|_| ())
        .expect_err("task recording must report its failure");
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    let patches = Patches::open(&stored, ReadOnly).expect("patches should open");
    assert_eq!(patches.all().expect("patches should list").count(), 1);
    assert_eq!(
        queue
            .get(task)
            .await
            .expect("failed writes must preserve saved context")
            .context,
        saved_context
    );
    queue
        .database()
        .execute("DROP TRIGGER fail_task_context_write")
        .await
        .expect("context writes should recover");
    coder.scope.deactivate().await;
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    let published = result_id(
        coder
            .create_patch(Parameters(request()))
            .await
            .expect("retry should recover the recorded mutation"),
    );
    let resumed = queue.get(task).await.expect("task context should read");
    assert_eq!(resumed.source, source.into());
    assert_eq!(
        resumed
            .context
            .patch()
            .expect("patch relationship should recover")
            .id()
            .to_string(),
        published
    );
    assert_eq!(patches.all().expect("patches should list").count(), 1);
}

#[tokio::test]
async fn untrusted_discussion_references_cannot_extend_task_write_access() {
    use crate::{agent::ActivityId, queue::TaskRequest};

    let fixture = RepositoryFixture::new();
    let owner = fixture.service().await;
    let issue = |title: &str| CreateIssueRequest {
        title: title.to_owned(),
        description: "Independent task".to_owned(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let source = Subject::Issue(
        ObjectId::from_str(&result_id(
            owner
                .create_issue_write(issue("Source"))
                .await
                .expect("source should create"),
        ))
        .expect("issue should parse"),
    );
    let target = Subject::Issue(
        ObjectId::from_str(&result_id(
            owner
                .create_issue_write(issue("Target"))
                .await
                .expect("target should create"),
        ))
        .expect("issue should parse"),
    );
    let outsider = fixture
        .service_as(
            Member::Maya,
            Role::Talker,
            TaskQueue::memory(fixture.repository, Member::Maya)
                .await
                .expect("queue should open"),
        )
        .await;
    let reference = || CommentIssueRequest {
        issue: source.id().to_string(),
        body: format!("Change {target}"),
        reply_to: None,
        embeds: Vec::new(),
    };
    outsider
        .comment_issue_write(reference())
        .await
        .expect("anyone may comment");
    let queue = fixture.queue().await;
    let task = queue
        .push(TaskRequest::new(
            "source only".to_owned(),
            source.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    let coder = fixture.service_with(Role::Coder, queue).await;
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    assert!(
        coder
            .access_subject(&SubjectRequest::from(target))
            .await
            .is_err(),
        "an untrusted comment cannot enlarge the task"
    );
    let reverse = fixture
        .service_with(Role::Talker, fixture.queue().await)
        .await;
    fixture
        .activate(
            &reverse,
            target,
            crate::agent::DeliveryId {
                operation: EntryId::from_str(&target.id().to_string())
                    .expect("operation should parse"),
                action: 0,
            }
            .into(),
        )
        .await;
    reverse
        .access_subject(&SubjectRequest::from(source))
        .await
        .expect_err("an untrusted reference cannot authorize the reverse relationship either");
    owner
        .comment_issue_write(reference())
        .await
        .expect("delegate should link authorized work");
    coder
        .access_subject(&SubjectRequest::from(target))
        .await
        .expect("the delegate's relationship should be accepted");
    reverse
        .access_subject(&SubjectRequest::from(source))
        .await
        .expect("a trusted discussion link must work in either direction");
}

#[tokio::test]
async fn an_implementation_reference_cannot_be_promoted_to_a_produced_patch() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::ActivityId,
        queue::{Artifact, ArtifactRole, ContextUpdate, TaskRequest},
    };

    let fixture = RepositoryFixture::new();
    let owner = fixture.service().await;
    let source = Subject::Issue(
        ObjectId::from_str(&result_id(
            owner
                .create_issue_write(CreateIssueRequest {
                    title: "Source".to_owned(),
                    description: "Independent task".to_owned(),
                    labels: Vec::new(),
                    assignees: Vec::new(),
                })
                .await
                .expect("source should create"),
        ))
        .expect("issue should parse"),
    );
    let request = || CreatePatchRequest {
        title: "Other task".to_owned(),
        description: "Unrelated implementation".to_owned(),
        base: fixture.base.to_string(),
        head: fixture.head.to_string(),
        labels: Vec::new(),
        draft: false,
    };
    let unrelated = result_id(
        owner
            .create_patch_write(request(), fixture.working.clone())
            .await
            .expect("patch should create"),
    );
    let queue = fixture.queue().await;
    let task = queue
        .push(TaskRequest::new(
            "source only".to_owned(),
            source.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    coder
        .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
            task: None,
            update: ContextUpdate {
                artifacts: vec![Artifact {
                    subject: format!("patch:{unrelated}"),
                    role: ArtifactRole::Implementation,
                    purpose: "Reference only".to_owned(),
                }],
                ..ContextUpdate::default()
            },
        }))
        .await
        .expect("saving context should work");
    coder
        .create_patch(Parameters(request()))
        .await
        .map(|_| ())
        .expect_err("reuse must verify the relationship first");
    assert!(
        queue
            .get(task)
            .await
            .expect("task should read")
            .context
            .patch()
            .is_none()
    );
}

#[tokio::test]
async fn a_cancelled_coder_task_cannot_publish_or_correct_another_task() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::ActivityId,
        queue::{ContextUpdate, TaskRequest},
    };

    let fixture = RepositoryFixture::new();
    let source = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("source should parse"),
    );
    let queue = fixture.queue().await;
    let request = || TaskRequest::new("work".to_owned(), source.into(), QueuePosition::End);
    let active = queue
        .push(request())
        .await
        .expect("task should record")
        .task;
    let other = queue
        .push(request())
        .await
        .expect("another task should record")
        .task;
    queue.next().await.expect("first task should start");
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, source, ActivityId::Task(active))
        .await;
    coder
        .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
            task: Some(other),
            update: ContextUpdate {
                specification: Some("Replace unrelated work".to_owned()),
                ..ContextUpdate::default()
            },
        }))
        .await
        .map(|_| ())
        .expect_err("the coder cannot change a different task sharing its source");
    queue.cancel(active).await.expect("task should stop");
    assert!(
        queue
            .running()
            .await
            .expect("task should await preservation")
            .context
            .cancelled
    );
    coder
        .access_subject(&SubjectRequest::from(source))
        .await
        .expect_err("cancelled work cannot write to its original subject");
    coder
        .create_issue(Parameters(CreateIssueRequest {
            title: "Unwanted replacement".to_owned(),
            description: "Stop should prevent publication".to_owned(),
            labels: Vec::new(),
            assignees: Vec::new(),
        }))
        .await
        .map(|_| ())
        .expect_err("cancelled work cannot create an issue");
    assert!(
        coder
            .get_task_context(Parameters(super::protocol::ContextRequest { task: None }))
            .await
            .expect("the stopped task can read its context")
            .0
            .context
            .cancelled
    );
    assert_eq!(
        queue
            .get(other)
            .await
            .expect("unrelated work remains")
            .context
            .specification,
        "work"
    );
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        Issues::open(&stored, ReadOnly)
            .expect("issues should open")
            .all()
            .expect("issues should list")
            .count(),
        0
    );
}

#[tokio::test]
async fn pins_real_specifications_and_revisions_and_rejects_missing_evidence_atomically() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::ActivityId,
        queue::{ContextUpdate, MessageReference, TaskRequest},
    };

    let fixture = RepositoryFixture::new();
    let owner = fixture.service().await;
    let specification = result_id(
        owner
            .create_issue_write(CreateIssueRequest {
                title: "Agreed scope".to_owned(),
                description: "Scaffold only".to_owned(),
                labels: Vec::new(),
                assignees: Vec::new(),
            })
            .await
            .expect("specification should create"),
    );
    let source = Subject::Issue(ObjectId::from_str(&specification).expect("issue should parse"));
    let revision = result_id(
        owner
            .create_patch_write(
                CreatePatchRequest {
                    title: "Scaffold".to_owned(),
                    description: format!("Implements {source}"),
                    base: fixture.base.to_string(),
                    head: fixture.head.to_string(),
                    labels: Vec::new(),
                    draft: false,
                },
                fixture.working.clone(),
            )
            .await
            .expect("revision should create"),
    );
    let patch = Subject::Patch(ObjectId::from_str(&revision).expect("patch should parse"));
    let queue = fixture.queue().await;
    let task = queue
        .push(TaskRequest::new(
            "scaffold".to_owned(),
            source.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    let messages = vec![
        MessageReference {
            subject: source.to_string(),
            entry: specification.clone(),
            revision: None,
            purpose: "Agreed scope".to_owned(),
        },
        MessageReference {
            subject: patch.to_string(),
            entry: revision.clone(),
            revision: Some(revision.clone()),
            purpose: "Reviewed implementation".to_owned(),
        },
    ];
    coder
        .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
            task: None,
            update: ContextUpdate {
                messages: messages.clone(),
                ..ContextUpdate::default()
            },
        }))
        .await
        .expect("existing evidence should pin");
    let before = queue.get(task).await.expect("context should read").context;
    for invalid in [
        MessageReference {
            entry: "3333333333333333333333333333333333333333".to_owned(),
            ..messages[0].clone()
        },
        MessageReference {
            revision: Some(specification.clone()),
            ..messages[1].clone()
        },
        MessageReference {
            revision: Some(specification.clone()),
            ..messages[0].clone()
        },
    ] {
        coder
            .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
                task: None,
                update: ContextUpdate {
                    specification: Some("Unverified replacement".to_owned()),
                    messages: vec![invalid],
                    ..ContextUpdate::default()
                },
            }))
            .await
            .map(|_| ())
            .expect_err("missing or mismatched evidence must be refused");
        assert_eq!(
            queue
                .get(task)
                .await
                .expect("context should remain")
                .context,
            before
        );
    }
    coder
        .update_task_context(Parameters(super::protocol::ContextUpdateRequest {
            task: None,
            update: ContextUpdate {
                messages,
                ..ContextUpdate::default()
            },
        }))
        .await
        .expect("saving the same evidence should reuse it");
    assert_eq!(
        queue
            .get(task)
            .await
            .expect("context should read")
            .context
            .messages
            .len(),
        2
    );
}

#[tokio::test]
async fn conversation_corrections_target_waiting_work_and_preserve_unrelated_tasks() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        agent::{ActivityId, DeliveryId},
        queue::TaskRequest,
    };

    let fixture = RepositoryFixture::new();
    let source = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("source should parse"),
    );
    let other = Subject::Issue(
        ObjectId::from_str("2222222222222222222222222222222222222222")
            .expect("other issue should parse"),
    );
    let queue = fixture.queue().await;
    let first_request = || {
        let mut request =
            TaskRequest::new("first task".to_owned(), source.into(), QueuePosition::End);
        request.request = Some("operation:first".to_owned());
        request
    };
    let first = queue
        .push(first_request())
        .await
        .expect("first task should record")
        .task;
    let second = queue
        .push(TaskRequest::new(
            "other task".to_owned(),
            other.into(),
            QueuePosition::End,
        ))
        .await
        .expect("other task should record")
        .task;
    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    fixture
        .activate(
            &talker,
            source,
            ActivityId::from(DeliveryId {
                operation: EntryId::from_str(&source.id().to_string())
                    .expect("operation should parse"),
                action: 0,
            }),
        )
        .await;
    talker
        .note(Parameters(super::protocol::NoteRequest {
            task: first,
            note: "Keep the reviewed head".to_owned(),
        }))
        .await
        .expect("waiting correction should save");
    talker
        .note(Parameters(super::protocol::NoteRequest {
            task: second,
            note: "Replace its scope".to_owned(),
        }))
        .await
        .map(|_| ())
        .expect_err("another issue cannot correct unrelated work");
    talker
        .cancel(Parameters(super::protocol::TaskRequest { task: second }))
        .await
        .map(|_| ())
        .expect_err("another issue cannot cancel unrelated work");
    let cancelled = talker
        .cancel(Parameters(super::protocol::TaskRequest { task: first }))
        .await
        .expect("explanation-only correction should cancel waiting work");
    assert_eq!(cancelled.0.task.task, first);
    assert!(cancelled.0.task.context.cancelled);
    assert_eq!(cancelled.0.task.context.notes, ["Keep the reviewed head"]);
    assert_eq!(
        queue
            .push(first_request())
            .await
            .expect("replay should retain cancellation")
            .state,
        crate::queue::TaskState::Cancelled
    );
    assert_eq!(
        queue
            .get(second)
            .await
            .expect("unrelated work remains")
            .context
            .specification,
        "other task"
    );
    assert!(
        queue
            .get(second)
            .await
            .expect("unrelated work remains")
            .context
            .notes
            .is_empty()
    );
    assert_eq!(queue.waiting().await.expect("queue should read"), 1);
}

#[tokio::test]
async fn concurrent_roles_reuse_one_issue_for_the_same_finding() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::agent::{ActivityId, DeliveryId};

    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let talker = fixture.service_with(Role::Talker, queue.clone()).await;
    let chatter = fixture.service_with(Role::Chatter, queue).await;
    let source = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("source should parse"),
    );
    let operation = EntryId::from_str(&source.id().to_string()).expect("operation should parse");
    fixture
        .activate(
            &talker,
            source,
            ActivityId::from(DeliveryId {
                operation,
                action: 0,
            }),
        )
        .await;
    fixture
        .activate(
            &chatter,
            source,
            ActivityId::from(DeliveryId {
                operation,
                action: 1,
            }),
        )
        .await;
    let request = || {
        Parameters(CreateIssueRequest {
            title: "One confirmed finding".to_owned(),
            description: "Independent evidence".to_owned(),
            labels: Vec::new(),
            assignees: Vec::new(),
        })
    };
    let (first, second) = tokio::join!(
        talker.create_issue(request()),
        chatter.create_issue(request())
    );
    assert_eq!(
        result_id(first.expect("talker publication should succeed")),
        result_id(second.expect("chatter publication should reuse the issue"))
    );
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository should open");
    assert_eq!(
        Issues::open(&stored, ReadOnly)
            .expect("issues should open")
            .all()
            .expect("issues should list")
            .count(),
        1
    );
}

#[tokio::test]
async fn keeps_an_active_tool_request_usable_while_deactivation_waits() {
    use crate::{agent::ActivityId, queue::TaskRequest};

    let fixture = RepositoryFixture::new();
    let source = Subject::Issue(
        ObjectId::from_str("1111111111111111111111111111111111111111")
            .expect("source should parse"),
    );
    let patch = Subject::Patch(
        ObjectId::from_str("2222222222222222222222222222222222222222").expect("patch should parse"),
    );
    let queue = fixture.queue().await;
    let task = queue
        .push(TaskRequest::new(
            "work".to_owned(),
            source.into(),
            QueuePosition::End,
        ))
        .await
        .expect("task should record")
        .task;
    queue
        .produced(task, patch)
        .await
        .expect("runtime artifact should record");
    let coder = fixture.service_with(Role::Coder, queue).await;
    fixture
        .activate(&coder, source, ActivityId::Task(task))
        .await;
    let access = coder
        .active()
        .await
        .expect("tool should acquire its active lease");
    let started = Arc::new(tokio::sync::Notify::new());
    let shutdown_started = Arc::clone(&started);
    let scope = coder.scope.clone();
    let shutdown = tokio::spawn(async move {
        shutdown_started.notify_one();
        scope.deactivate().await;
    });
    started.notified().await;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        coder
            .verify_subject(&access, &SubjectRequest::from(source))
            .await
            .expect("the original subject remains usable");
        assert_eq!(
            coder
                .existing_patch(&access)
                .await
                .expect("existing artifact verification must reuse the lease"),
            Some(patch.id())
        );
        assert_eq!(
            access
                .task_id(None)
                .expect("task selection must use the existing lease"),
            task
        );
        assert_eq!(
            coder
                .task_access(&access, task)
                .await
                .expect("context validation must use the existing lease")
                .id,
            task
        );
    })
    .await;
    drop(access);
    finished.expect("a queued scope writer must not deadlock an already active tool call");
    tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
        .await
        .expect("deactivation should finish when the lease is released")
        .expect("shutdown task should finish");
    coder
        .active()
        .await
        .expect_err("the deactivated scope rejects new tool requests");
}

#[tokio::test]
async fn queued_runtime_receipts_are_signed_and_retry_without_duplicate_reactions() {
    use radicle::cob::stream::CobStream as _;

    use crate::{
        delivery::{State as DeliveryState, remote::Deliveries},
        watcher::{Activity, Kind},
    };
    let fixture = RepositoryFixture::new();
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository");
    let signer = fixture.profile.signer().expect("signer");
    let issue = {
        let mut issues = fixture
            .profile
            .issues_mut(&stored, &signer)
            .expect("issue store");
        *issues
            .create(
                "Queued".to_owned().try_into().expect("title"),
                "Work",
                &[],
                &[],
                [],
            )
            .expect("issue")
            .id()
    };
    let queue = fixture.queue().await;
    let inbox = queue.inbox().await.expect("inbox");
    let activity = Activity {
        repository: fixture.repository,
        subject: Subject::Issue(issue),
        operation: *issue,
        action: 0,
        author: fixture.profile.did(),
        kind: Kind::Assigned,
    };
    let task = queue
        .push_assignment(
            crate::queue::TaskRequest::new(
                "Work".into(),
                activity.subject.into(),
                crate::manifest::QueuePosition::End,
            ),
            activity.clone(),
        )
        .await
        .expect("atomic admission");
    assert_eq!(inbox.list().await.expect("inbox")[0].task, Some(task.task));
    let announcements = Arc::new(AtomicUsize::new(0));
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        FlakyAnnouncer(announcements.clone()),
        RotatingAuthorizer(Arc::new(tokio::sync::RwLock::new(
            crate::manifest::SecurityPolicy::test(fixture.repository),
        ))),
    )
    .with_memory_journal()
    .await
    .expect("journal");
    publisher
        .receipt(&activity, Member::Nico.receipt())
        .await
        .expect_err("first announcement fails");
    publisher
        .reconcile(activity.delivery_id().into())
        .await
        .expect("model publications recover independently");
    assert_eq!(
        announcements.load(Ordering::Relaxed),
        1,
        "a model's recovery must not handle runtime receipts"
    );
    publisher
        .receipt(&activity, Member::Nico.receipt())
        .await
        .expect("retry signed receipt");
    let operations = radicle::cob::issue::IssueStream::init(issue, &stored)
        .all()
        .expect("history")
        .count();
    assert_eq!(
        operations, 2,
        "receipt retry cannot append another reaction"
    );
    let publish = inbox.publish(&publisher, Member::Nico, Member::Nico.receipt());
    tokio::pin!(publish);
    let exercise = async {
        while !inbox.pending().await.expect("outbox").is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let store = radicle::cob::store::Store::<Deliveries, _, _>::open(&stored, ReadOnly)
            .expect("delivery store");
        let records = store
            .all()
            .expect("records")
            .collect::<Result<Vec<_>, _>>()
            .expect("signed COBs");
        assert_eq!(records.len(), 1);
        let record = inbox.list().await.expect("inbox").remove(0);
        assert_eq!(records[0].1.author, *fixture.profile.id());
        assert_eq!(
            records[0].1.records[&record.id()].state,
            DeliveryState::Queued
        );
        inbox
            .set(&record.id(), DeliveryState::Handled, None)
            .await
            .expect("complete");
        while !inbox.pending().await.expect("outbox").is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let records = store
            .all()
            .expect("records")
            .collect::<Result<Vec<_>, _>>()
            .expect("updated COBs");
        assert_eq!(
            records.len(),
            1,
            "state changes update the member's existing COB"
        );
        assert_eq!(
            records[0].1.records[&record.id()].state,
            DeliveryState::Handled
        );
        fixture.bind_receipts(fixture.profile.did());
        assert_eq!(
            Deliveries::read(&stored)
                .expect("verified remote read")
                .len(),
            1
        );
        fixture.bind_receipts(
            "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
                .parse()
                .expect("other DID"),
        );
        assert!(
            Deliveries::read(&stored)
                .expect("mismatched signing DID")
                .is_empty(),
            "only the canonical member binding may claim these receipts"
        );
        assert!(
            radicle::storage::refs::SignedRefs::load(*fixture.profile.id(), &stored)
                .expect("signed refs")
                .expect("signature")
                .iter()
                .any(|(name, _)| name.to_string().contains("xyz.radicle.team.delivery"))
        );
    };
    tokio::select! {
        result = &mut publish => panic!("receipt worker ended: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), exercise) => result.expect("publication should complete without a model"),
    }
}

#[tokio::test]
async fn a_missing_reaction_target_does_not_hide_signed_queue_admission() {
    use crate::{
        delivery::{State, remote::Deliveries},
        watcher::{Activity, Delivery, Kind, Target},
    };
    let fixture = RepositoryFixture::new();
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository");
    let inbox = fixture.queue().await.inbox().await.expect("inbox");
    let missing = "1111111111111111111111111111111111111111"
        .parse()
        .expect("missing issue");
    let activity = Activity {
        repository: fixture.repository,
        subject: Subject::Issue(missing),
        operation: *missing,
        action: 0,
        author: fixture.profile.did(),
        kind: Kind::Delivered(Delivery::Mentioned(Target::Subject)),
    };
    inbox.enqueue(activity, None).await.expect("admission");
    let publisher = crate::agent::Publisher::new(
        fixture.profile.clone(),
        fixture.repository,
        None,
        TestAnnouncer,
        RotatingAuthorizer(Arc::new(tokio::sync::RwLock::new(
            crate::manifest::SecurityPolicy::test(fixture.repository),
        ))),
    )
    .with_memory_journal()
    .await
    .expect("journal");
    let publish = inbox.publish(&publisher, Member::Nico, Member::Nico.receipt());
    let exercise = async {
        let store = radicle::cob::store::Store::<Deliveries, _, _>::open(&stored, ReadOnly)
            .expect("delivery store");
        loop {
            let records = store
                .all()
                .expect("records")
                .collect::<Result<Vec<_>, _>>()
                .expect("signed records");
            if let Some((_, delivery)) = records.first() {
                assert_eq!(
                    delivery.records.values().next().expect("record").state,
                    State::Queued
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let pending = inbox.pending().await.expect("outbox");
        assert_eq!(
            pending.len(),
            1,
            "the unavailable native reaction remains retryable"
        );
        assert!(!pending[0].1);
    };
    tokio::select! {
        result = publish => panic!("receipt worker ended: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), exercise) => result.expect("signed delivery state must not wait for the reaction"),
    }
}

#[tokio::test]
async fn identical_comments_reuse_the_existing_entry_but_distinct_replies_stay_separate() {
    let fixture = RepositoryFixture::new();
    let service = fixture.service().await;
    let issue = service
        .create_issue_write(CreateIssueRequest {
            title: "Do it".into(),
            description: "Request".into(),
            labels: Vec::new(),
            assignees: Vec::new(),
        })
        .await
        .expect("issue");
    let issue_id = issue.0.data["id"].as_str().expect("issue ID");
    let request = |reply: Option<&str>| CommentWriteRequest {
        subject: RepositoryFixture::subject(SubjectKind::Issue, issue_id),
        target: CommentTargetKind::Issue,
        entity: None,
        comment: None,
        body: Some("Done".into()),
        reply_to: reply.map(str::to_owned),
        location: None,
        embeds: Vec::new(),
    };
    let first = service
        .write_comment(request(None), CommentOperation::Create)
        .await
        .expect("comment");
    let repeated = service
        .write_comment(request(None), CommentOperation::Create)
        .await
        .expect("reuse");
    assert_eq!(first.0.data, repeated.0.data);
    let different = service
        .write_comment(
            request(first.0.data["id"].as_str()),
            CommentOperation::Create,
        )
        .await
        .expect("distinct reply target");
    assert_ne!(first.0.data, different.0.data);
}

#[tokio::test]
async fn runtime_receipts_acknowledge_the_actual_review_and_comment_targets() {
    use crate::watcher::{Activity, Change, Comment, Delivery, Kind};
    let fixture = RepositoryFixture::new();
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("repository");
    let signer = fixture.profile.signer().expect("signer");
    let (id, revision, review, review_comment, revision_comment) = {
        let mut patches = fixture
            .profile
            .patches_mut(&stored, &signer)
            .expect("patch store");
        let mut patch = patches
            .create(
                "Changes".to_owned().try_into().expect("title"),
                "Changes",
                patch::MergeTarget::Delegates,
                Oid::from(fixture.base),
                Oid::from(fixture.head),
                &[],
            )
            .expect("patch");
        let revision = patch.latest().0;
        let review = patch
            .review(
                revision,
                Some(patch::Verdict::Accept),
                Some("An optional suggestion".into()),
                Vec::new(),
            )
            .expect("review");
        let review_comment = patch
            .review_comment(review, "Suggestion", None, None, [])
            .expect("review comment");
        let revision_comment = patch
            .comment(revision, "Question", None, None, [])
            .expect("revision comment");
        (
            *patch.id(),
            revision,
            review,
            review_comment,
            revision_comment,
        )
    };
    let publisher = fixture.service().await.publisher;
    let emoji = Member::Maya.receipt();
    let events = [
        (review.into(), Change::ReviewSubmitted { review, revision }),
        (
            review_comment,
            Change::CommentCreated(Comment::Review {
                review,
                comment: review_comment,
            }),
        ),
        (
            revision_comment,
            Change::CommentCreated(Comment::Revision {
                revision,
                comment: revision_comment,
            }),
        ),
        (revision.into(), Change::RevisionCreated(revision)),
    ];
    for (operation, change) in events {
        let activity = Activity {
            repository: fixture.repository,
            subject: Subject::Patch(id),
            operation,
            action: 0,
            author: fixture.profile.did(),
            kind: Kind::Delivered(Delivery::Changed(change)),
        };
        publisher
            .receipt(&activity, emoji)
            .await
            .expect("targeted receipt");
        publisher
            .receipt(&activity, emoji)
            .await
            .expect("idempotent receipt");
    }
    let patch = Patches::open(&stored, ReadOnly)
        .expect("patches")
        .get(&id)
        .expect("read patch")
        .expect("patch");
    let reaction = radicle::cob::Reaction::new(emoji).expect("reaction");
    let review = super::recovery::find_review(&patch, review).expect("review");
    assert!(
        review
            .reactions()
            .contains(&(*fixture.profile.id(), reaction))
    );
    assert!(
        review
            .comments()
            .find(|(id, _)| **id == review_comment)
            .expect("comment")
            .1
            .reactions()
            .get(&reaction)
            .is_some_and(|authors| authors.contains(&fixture.profile.id()))
    );
    let revision = patch.revision(&revision).expect("revision");
    assert!(
        revision
            .reactions()
            .get(&None)
            .expect("revision receipt")
            .contains(&(*fixture.profile.id(), reaction))
    );
    assert!(
        revision
            .discussion()
            .comment(&revision_comment)
            .expect("comment")
            .reactions()
            .get(&reaction)
            .is_some_and(|authors| authors.contains(&fixture.profile.id()))
    );
}

#[tokio::test]
async fn linked_patch_is_readable_but_requires_an_explicit_review_invitation() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::watcher::{Comment, Delivery, Kind, Target};
    let fixture = RepositoryFixture::new();
    let operator = fixture.service().await;
    let patch = fixture.patch(&operator).await;
    let issue_id = result_id(
        operator
            .create_issue_write(CreateIssueRequest {
                title: "Origin".into(),
                description: format!("Implemented in {patch}"),
                labels: vec![],
                assignees: vec![],
            })
            .await
            .expect("issue"),
    );
    let issue = Subject::Issue(issue_id.parse().expect("issue ID"));
    let comment = result_id(
        operator
            .comment_issue_write(CommentIssueRequest {
                issue: issue_id,
                body: format!("The implementation is in {patch}"),
                reply_to: None,
                embeds: vec![],
            })
            .await
            .expect("link"),
    );
    let operation: EntryId = comment.parse().expect("comment ID");
    let maya = fixture
        .service_as(Member::Maya, Role::Talker, fixture.queue().await)
        .await;
    fixture
        .deliver(
            &maya,
            fixture.activity(
                issue,
                operation,
                Kind::Delivered(Delivery::Replied {
                    comment: Comment::Issue(operation),
                    reply_to: *issue.id(),
                }),
            ),
        )
        .await;
    maya.get_subject(Parameters(SubjectRequest::from(patch)))
        .await
        .expect("linked patch is readable");
    let review = || ReviewWriteRequest {
        subject: SubjectRequest::from(patch),
        revision: Some(patch.id().to_string()),
        review: None,
        verdict: Some("accept".into()),
        summary: Some("Scope checked".into()),
        labels: vec![],
        embeds: vec![],
        comments: vec![],
    };
    let error = maya
        .create_review(Parameters(review()))
        .await
        .err()
        .expect("a link is not a review invitation");
    assert!(error.contains("does not invite a review"), "{error}");
    let invited = result_id(
        operator
            .comment_issue_write(CommentIssueRequest {
                issue: issue.id().to_string(),
                body: format!("@maya please review {patch}"),
                reply_to: None,
                embeds: vec![],
            })
            .await
            .expect("invitation"),
    );
    let invitation: EntryId = invited.parse().expect("invitation ID");
    fixture
        .deliver(
            &maya,
            fixture.activity(
                issue,
                invitation,
                Kind::Delivered(Delivery::Mentioned(Target::Comment(Comment::Issue(
                    invitation,
                )))),
            ),
        )
        .await;
    maya.create_review(Parameters(review()))
        .await
        .expect("explicitly addressed review may publish");
}

#[tokio::test]
async fn reviewer_follows_only_own_findings_and_cannot_implement_other_reviews() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        queue::TaskKind,
        watcher::{Change, Comment, Delivery, Kind, Target},
    };
    let fixture = RepositoryFixture::new();
    let queue = fixture.queue().await;
    let owner = fixture
        .service_as(Member::Maya, Role::Talker, fixture.queue().await)
        .await;
    let patch = fixture.patch(&owner).await;
    let delegate = fixture.service().await;
    let reviewer = fixture
        .service_as(Member::Nico, Role::Talker, queue.clone())
        .await;
    let assigned = result_id(
        delegate
            .set_strings_write(
                SetStringsRequest {
                    subject: SubjectRequest::from(patch),
                    values: vec![format!("did:key:{}", reviewer.publisher.author())],
                },
                SubjectKind::Patch,
                StringUpdate::Assignees,
            )
            .await
            .expect("assignment"),
    );
    let assignment: EntryId = assigned.parse().expect("assignment ID");
    fixture
        .deliver(
            &reviewer,
            fixture.activity(patch, assignment, Kind::Assigned),
        )
        .await;
    let review_request = |verdict: &str, revision: String, previous| ReviewWriteRequest {
        subject: SubjectRequest::from(patch),
        revision: Some(revision),
        review: previous,
        verdict: Some(verdict.into()),
        summary: Some("Check the requested behavior".into()),
        labels: vec![],
        embeds: vec![],
        comments: vec![],
    };
    let queued = reviewer
        .add_task(Parameters(super::protocol::AddTaskRequest {
            brief: "Review this patch".into(),
            kind: None,
            position: QueuePosition::End,
            context: crate::queue::ContextUpdate::default(),
            distinct: false,
        }))
        .await
        .expect("review queued")
        .0
        .task;
    assert_eq!(
        queue.get(queued).await.expect("task").kind(),
        TaskKind::Review
    );
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, patch, crate::agent::ActivityId::Task(queued))
        .await;
    coder
        .create_patch(Parameters(CreatePatchRequest {
            title: "Unauthorized implementation".into(),
            description: "Wrong role".into(),
            base: fixture.base.to_string(),
            head: fixture.head.to_string(),
            labels: vec![],
            draft: true,
        }))
        .await
        .err()
        .expect("review work cannot publish implementation");
    coder
        .set_patch_state(Parameters(SetStateRequest {
            subject: SubjectRequest::from(patch),
            state: "open".into(),
        }))
        .await
        .err()
        .expect("review work cannot mark implementation ready");
    reviewer
        .add_task(Parameters(super::protocol::AddTaskRequest {
            brief: "Implement somebody else's requested changes".into(),
            kind: Some(TaskKind::Implementation),
            position: QueuePosition::End,
            context: crate::queue::ContextUpdate::default(),
            distinct: true,
        }))
        .await
        .err()
        .expect("review assignment cannot authorize implementation");
    let own = result_id(
        reviewer
            .create_review(Parameters(review_request(
                "accept",
                patch.id().to_string(),
                None,
            )))
            .await
            .expect("initial invited review"),
    );
    let mut delegate_review = review_request("reject", patch.id().to_string(), None);
    delegate_review.comments.push(ReviewCommentRequest {
        body: "Please correct this behavior".into(),
        location: None,
    });
    let other = result_id(
        delegate
            .review_write(delegate_review, false)
            .await
            .expect("delegate review"),
    );
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("storage");
    let state = Patches::open(&stored, ReadOnly)
        .expect("patches")
        .get(&patch.id())
        .expect("patch")
        .expect("patch exists");
    let other_id = other.parse::<EntryId>().expect("review ID").into();
    let reply_to = find_review(&state, other_id)
        .expect("delegate review")
        .comments()
        .next()
        .expect("finding")
        .0
        .to_string();
    let revision = result_id(
        owner
            .update_patch_write(
                super::protocol::UpdatePatchRequest {
                    subject: SubjectRequest::from(patch),
                    description: "Delegate's feedback addressed".into(),
                    base: fixture.base.to_string(),
                    head: fixture.head.to_string(),
                },
                fixture.working.clone(),
            )
            .await
            .expect("new revision"),
    );
    fixture
        .deliver(
            &reviewer,
            fixture.activity(
                patch,
                revision.parse().expect("revision"),
                Kind::Delivered(Delivery::Changed(Change::RevisionCreated(
                    revision.parse::<EntryId>().expect("revision").into(),
                ))),
            ),
        )
        .await;
    reviewer
        .create_review(Parameters(review_request("accept", revision.clone(), None)))
        .await
        .err()
        .expect("accepted reviewer must not claim another reviewer's fixes");
    reviewer
        .create_comment(Parameters(CommentWriteRequest {
            subject: SubjectRequest::from(patch),
            target: CommentTargetKind::Review,
            entity: Some(other.clone()),
            comment: None,
            body: Some("I checked your fixes".into()),
            reply_to: Some(reply_to.clone()),
            location: None,
            embeds: vec![],
        }))
        .await
        .err()
        .expect("another review's thread needs direct address");
    reviewer
        .review_write(
            ReviewWriteRequest {
                review: Some(own.clone()),
                ..review_request("reject", patch.id().to_string(), None)
            },
            true,
        )
        .await
        .expect("own verdict can be changed");
    let follow_up = fixture.activity(
        patch,
        revision.parse().expect("revision"),
        Kind::Delivered(Delivery::Changed(Change::ReviewFollowUp {
            review: own.parse::<EntryId>().expect("review").into(),
            revision: revision.parse::<EntryId>().expect("revision").into(),
        })),
    );
    fixture.deliver(&reviewer, follow_up).await;
    reviewer
        .create_review(Parameters(review_request(
            "accept",
            revision.clone(),
            Some(other.clone()),
        )))
        .await
        .err()
        .expect("own follow-up cannot substitute another review ID");
    reviewer
        .create_review(Parameters(review_request(
            "accept",
            revision.clone(),
            Some(own),
        )))
        .await
        .expect("own rejected findings may be checked on the new revision");
    let addressed = result_id(
        delegate
            .write_comment(
                CommentWriteRequest {
                    subject: SubjectRequest::from(patch),
                    target: CommentTargetKind::Review,
                    entity: Some(other.clone()),
                    comment: None,
                    body: Some("@nico please clarify this finding".into()),
                    reply_to: Some(reply_to),
                    location: None,
                    embeds: vec![],
                },
                CommentOperation::Create,
            )
            .await
            .expect("direct request"),
    );
    let addressed_id: EntryId = addressed.parse().expect("request ID");
    fixture
        .deliver(
            &reviewer,
            fixture.activity(
                patch,
                addressed_id,
                Kind::Delivered(Delivery::Mentioned(Target::Comment(Comment::Review {
                    review: other.parse::<EntryId>().expect("review").into(),
                    comment: addressed_id,
                }))),
            ),
        )
        .await;
    reviewer
        .create_comment(Parameters(CommentWriteRequest {
            subject: SubjectRequest::from(patch),
            target: CommentTargetKind::Review,
            entity: Some(other),
            comment: None,
            body: Some("Answer to the direct request".into()),
            reply_to: Some(addressed),
            location: None,
            embeds: vec![],
        }))
        .await
        .expect("direct address permits a reply in that review");
}

#[tokio::test]
async fn handoff_requires_receiving_members_signed_implementation_admission() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        queue::{TaskKind, TaskRequest},
        watcher::{Comment, Delivery, Kind, Target},
    };
    let fixture = RepositoryFixture::new();
    fixture.bind_receipts(fixture.profile.did());
    let receiver_queue = fixture.queue().await;
    let receiver = fixture
        .service_with(Role::Talker, receiver_queue.clone())
        .await;
    let sender_queue = TaskQueue::memory(fixture.repository, Member::Maya)
        .await
        .expect("sender queue");
    let sender = fixture
        .service_as(Member::Maya, Role::Talker, sender_queue.clone())
        .await;
    let issue = result_id(
        receiver
            .create_issue_write(CreateIssueRequest {
                title: "Shared work".into(),
                description: "An implementation task".into(),
                labels: vec![],
                assignees: vec![],
            })
            .await
            .expect("issue"),
    );
    let subject = Subject::Issue(issue.parse().expect("issue ID"));
    let task = sender_queue
        .push(TaskRequest::new(
            "Implementation".into(),
            subject.into(),
            QueuePosition::End,
        ))
        .await
        .expect("original promise")
        .task;
    let operation = result_id(
        sender
            .comment_issue_write(CommentIssueRequest {
                issue,
                body: "@nico please take over this implementation".into(),
                reply_to: None,
                embeds: vec![],
            })
            .await
            .expect("handoff message"),
    );
    let entry: EntryId = operation.parse().expect("operation");
    let mut activity = fixture.activity(
        subject,
        entry,
        Kind::Delivered(Delivery::Mentioned(Target::Comment(Comment::Issue(entry)))),
    );
    activity.author = sender.publisher.author().into();
    fixture.deliver(&sender, activity.clone()).await;
    let handoff = || super::protocol::HandoffRequest {
        task,
        member: "nico".into(),
        operation: operation.clone(),
        remaining: None,
    };
    sender
        .handoff_task(Parameters(handoff()))
        .await
        .err()
        .expect("message alone cannot transfer work");
    assert!(
        !sender_queue
            .get(task)
            .await
            .expect("original remains")
            .context
            .cancelled
    );
    let inbox = receiver_queue.inbox().await.expect("receiver inbox");
    inbox
        .enqueue(activity.clone(), None)
        .await
        .expect("notification receipt");
    fixture.publish_receipts(&inbox, &receiver.publisher).await;
    sender
        .handoff_task(Parameters(handoff()))
        .await
        .err()
        .expect("a notification receipt is not implementation acceptance");
    let request = |kind| {
        let mut request = TaskRequest::new(
            "Transferred work".into(),
            subject.into(),
            QueuePosition::Next,
        );
        request.request = Some("handoff-delivery".into());
        request.context.kind = kind;
        request.context.activity = Some(activity.clone());
        request
    };
    receiver_queue
        .push_assignment(request(TaskKind::Review), activity.clone())
        .await
        .expect("review queued");
    fixture.publish_receipts(&inbox, &receiver.publisher).await;
    sender
        .handoff_task(Parameters(handoff()))
        .await
        .err()
        .expect("review admission does not transfer implementation");
    receiver_queue
        .push_assignment(request(TaskKind::Implementation), activity.clone())
        .await
        .expect("implementation queued");
    fixture.publish_receipts(&inbox, &receiver.publisher).await;
    let transferred = sender
        .handoff_task(Parameters(handoff()))
        .await
        .expect("confirmed handoff");
    assert!(transferred.0.task.context.cancelled);
    assert!(matches!(
        sender_queue.get(task).await,
        Err(crate::queue::Error::MissingTask(_))
    ));
    assert_eq!(
        inbox.list().await.expect("receipt").len(),
        1,
        "queue changes update the existing receipt"
    );
}

#[tokio::test]
async fn resolved_findings_allow_followup_only_on_the_members_own_review() {
    use std::collections::BTreeSet;

    use rmcp::handler::server::wrapper::Parameters;

    use crate::watcher::{Change, Delivery, Kind, Target};
    let fixture = RepositoryFixture::new();
    let owner = fixture.service().await;
    let patch = fixture.patch(&owner).await;
    let reviewer = fixture
        .service_as(Member::Maya, Role::Talker, fixture.queue().await)
        .await;
    fixture
        .deliver(
            &reviewer,
            fixture.activity(
                patch,
                *patch.id(),
                Kind::Delivered(Delivery::Mentioned(Target::Revision((*patch.id()).into()))),
            ),
        )
        .await;
    let request = |revision: String, review| ReviewWriteRequest {
        subject: SubjectRequest::from(patch),
        revision: Some(revision),
        review,
        verdict: Some("accept".into()),
        summary: Some("Required behavior is sound".into()),
        labels: vec![],
        embeds: vec![],
        comments: vec![],
    };
    let mut initial = request(patch.id().to_string(), None);
    initial.comments.push(ReviewCommentRequest {
        body: "Optional simplification".into(),
        location: None,
    });
    let own = result_id(
        reviewer
            .create_review(Parameters(initial))
            .await
            .expect("initial review"),
    );
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("storage");
    let signer = fixture.profile.signer().expect("signer");
    let mut patches = patch::Cache::no_cache(&stored, &signer).expect("patch store");
    let mut object = patches.get_mut(&patch.id()).expect("patch");
    let review_id = own.parse::<EntryId>().expect("review ID").into();
    let comment = *find_review(&object, review_id)
        .expect("own review")
        .comments()
        .next()
        .expect("finding")
        .0;
    let revision = object
        .transaction("Resolve own finding", |tx| {
            tx.push(PatchAction::Revision {
                description: "Resolve the optional finding".into(),
                base: fixture.base.into(),
                oid: fixture.head.into(),
                resolves: BTreeSet::from([(own.parse().expect("review ID"), comment)]),
            })
        })
        .expect("resolved revision");
    fixture
        .deliver(
            &reviewer,
            fixture.activity(
                patch,
                revision,
                Kind::Delivered(Delivery::Changed(Change::ReviewCommentsResolved)),
            ),
        )
        .await;
    reviewer
        .create_review(Parameters(request(revision.to_string(), None)))
        .await
        .err()
        .expect("followup requires own review ID");
    reviewer
        .create_review(Parameters(request(
            revision.to_string(),
            Some(patch.id().to_string()),
        )))
        .await
        .err()
        .expect("unrelated review is denied");
    reviewer
        .add_task(Parameters(super::protocol::AddTaskRequest {
            brief: "Check my resolved finding".into(),
            kind: None,
            position: QueuePosition::End,
            context: crate::queue::ContextUpdate::default(),
            distinct: false,
        }))
        .await
        .expect("scoped review may enter the queue");
    reviewer
        .create_review(Parameters(request(revision.to_string(), Some(own))))
        .await
        .expect("own resolved finding may be checked");
}

#[tokio::test]
async fn implementation_reference_does_not_grant_another_authors_patch() {
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        queue::{Artifact, ArtifactRole, ContextUpdate, TaskRequest},
        watcher::{Comment, Delivery, Kind, Target},
    };
    let fixture = RepositoryFixture::new();
    let owner = fixture
        .service_as(Member::Maya, Role::Talker, fixture.queue().await)
        .await;
    let patch = fixture.patch(&owner).await;
    let delegate = fixture.service().await;
    let issue_id = result_id(
        delegate
            .create_issue_write(CreateIssueRequest {
                title: "Related task".into(),
                description: format!("Information from {patch}"),
                labels: vec![],
                assignees: vec![],
            })
            .await
            .expect("issue"),
    );
    let issue = Subject::Issue(issue_id.parse().expect("issue ID"));
    let queue = fixture.queue().await;
    let mut request = TaskRequest::new(
        "Own implementation".into(),
        issue.into(),
        QueuePosition::End,
    );
    request
        .context
        .update(ContextUpdate {
            artifacts: vec![Artifact {
                subject: patch.to_string(),
                role: ArtifactRole::Implementation,
                purpose: "Related code".into(),
            }],
            ..ContextUpdate::default()
        })
        .expect("reference");
    request.request = Some("original request".into());
    let task = queue.push(request).await.expect("task").task;
    let coder = fixture.service_with(Role::Coder, queue.clone()).await;
    fixture
        .activate(&coder, issue, crate::agent::ActivityId::Task(task))
        .await;
    let update = || super::protocol::UpdatePatchRequest {
        subject: SubjectRequest::from(patch),
        description: "Updated behavior".into(),
        base: fixture.base.to_string(),
        head: fixture.head.to_string(),
    };
    let error = coder
        .update_patch(Parameters(update()))
        .await
        .err()
        .expect("reference is not permission");
    assert!(error.contains("explicit implementation request"), "{error}");
    coder
        .set_patch_state(Parameters(SetStateRequest {
            subject: SubjectRequest::from(patch),
            state: "open".into(),
        }))
        .await
        .err()
        .expect("a saved reference cannot mark another author's patch ready");
    let operation = result_id(
        delegate
            .comment_issue_write(CommentIssueRequest {
                issue: issue_id,
                body: format!("@nico implement this change in {patch}"),
                reply_to: None,
                embeds: vec![],
            })
            .await
            .expect("explicit request"),
    );
    let operation: EntryId = operation.parse().expect("request ID");
    let activity = fixture.activity(
        issue,
        operation,
        Kind::Delivered(Delivery::Mentioned(Target::Comment(Comment::Issue(
            operation,
        )))),
    );
    let mut accepted = TaskRequest::new(
        "Explicit implementation handoff".into(),
        issue.into(),
        QueuePosition::Next,
    );
    accepted.context.activity = Some(activity);
    accepted.request = Some("explicit handoff".into());
    assert_eq!(queue.push(accepted).await.expect("correction").task, task);
    coder.scope.deactivate().await;
    fixture
        .activate(&coder, issue, crate::agent::ActivityId::Task(task))
        .await;
    coder
        .update_patch(Parameters(update()))
        .await
        .expect("explicitly addressed implementation may revise the patch");
    for state in ["draft", "open"] {
        coder
            .set_patch_state(Parameters(SetStateRequest {
                subject: SubjectRequest::from(patch),
                state: state.into(),
            }))
            .await
            .expect("an explicit implementation handoff may change readiness");
    }
    let stored = fixture
        .profile
        .storage
        .repository(fixture.repository)
        .expect("storage");
    let object = Patches::open(&stored, ReadOnly)
        .expect("patch store")
        .get(&patch.id())
        .expect("patch")
        .expect("patch exists");
    assert!(matches!(object.state(), patch::State::Open { .. }));
}
