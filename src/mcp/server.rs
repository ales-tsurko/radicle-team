//! Loopback HTTP transport for the MCP endpoint.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::Arc,
};

use radicle::{identity::RepoId, storage::git::Storage};
use rand::distr::{Alphanumeric, SampleString as _};
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    agent::{
        ActivityId, ChatContext, Context as AgentContext, McpEndpoint, Publisher, Role, Source,
    },
    manifest::McpAllowlist,
    mcp::{
        protocol::{SubjectKind, SubjectRequest},
        tools::{ToolService, ToolState},
    },
    watcher::Subject,
};

/// Embedded MCP server scoped to one Radicle profile and repository.
pub(crate) struct Server {
    endpoint: McpEndpoint,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    scope: ActivityScope,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("McpServer").finish_non_exhaustive()
    }
}

impl Server {
    pub(crate) async fn start(
        storage: Storage,
        repository: RepoId,
        publisher: Publisher,
        allowlist: McpAllowlist,
        role: Role,
        state: ToolState,
    ) -> Result<Self, Error> {
        let listener = Self::bind_loopback().await?;
        let address = listener.local_addr()?;
        let capability = Alphanumeric.sample_string(&mut rand::rng(), 48);
        let route = format!("/mcp/{capability}");
        let endpoint = McpEndpoint::new(Url::parse(&format!("http://{address}{route}"))?);
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.child_token();
        let scope = ActivityScope::new(publisher.clone());
        let task = tokio::spawn(Self::serve(
            listener,
            route,
            ToolService::new(
                storage,
                repository,
                scope.clone(),
                publisher,
                allowlist,
                role,
                state,
            ),
            server_cancellation,
        ));

        Ok(Self {
            endpoint,
            cancellation,
            task: Some(task),
            scope,
        })
    }

    pub(crate) const fn endpoint(&self) -> &McpEndpoint {
        &self.endpoint
    }

    async fn bind_loopback() -> std::io::Result<tokio::net::TcpListener> {
        match tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await {
            Ok(listener) => Ok(listener),
            Err(_ipv6_error) => tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await,
        }
    }

    pub(crate) fn scope(&self) -> ActivityScope {
        self.scope.clone()
    }

    pub(crate) async fn stop(mut self) -> Result<(), Error> {
        self.shutdown().await
    }

    async fn serve(
        listener: tokio::net::TcpListener,
        route: String,
        tools: ToolService,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let service: StreamableHttpService<ToolService, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(tools.clone()),
                Default::default(),
                StreamableHttpServerConfig::default()
                    .with_sse_keep_alive(None)
                    .with_cancellation_token(cancellation.child_token()),
            );
        let router = axum::Router::new().nest_service(&route, service);
        axum::serve(listener, router)
            .with_graceful_shutdown(cancellation.cancelled_owned())
            .await
            .map_err(Error::Io)
    }

    async fn shutdown(&mut self) -> Result<(), Error> {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| Error::Task(error.to_string()))??;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// Repository the member is addressing while one agent activity runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityContext {
    pub(crate) repository: RepoId,
    pub(crate) source: Source,
    pub(crate) activity: ActivityId,
    pub(crate) workspace: PathBuf,
}

impl From<AgentContext> for ActivityContext {
    fn from(context: AgentContext) -> Self {
        Self {
            repository: context.repository,
            source: context.subject.into(),
            activity: context.delivery.into(),
            workspace: context.workspace,
        }
    }
}

impl From<ChatContext> for ActivityContext {
    fn from(context: ChatContext) -> Self {
        Self {
            repository: context.repository(),
            source: Source::Chat(context.channel().clone()),
            activity: context.message().into(),
            workspace: context.workspace().to_owned(),
        }
    }
}

// Activates MCP access for exactly one delivery, authenticated chat message, or task.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActivityScope {
    active: Arc<tokio::sync::RwLock<Option<Arc<ActiveActivity>>>>,
    publisher: Option<Publisher>,
}

impl ActivityScope {
    pub(super) fn new(publisher: Publisher) -> Self {
        Self {
            active: Arc::default(),
            publisher: Some(publisher),
        }
    }

    pub(crate) async fn activate(&self, context: impl Into<ActivityContext>) -> Result<(), String> {
        let mut active = self.active.write().await;
        if active.is_some() {
            return Err("another activity is already active".to_owned());
        }
        *active = Some(Arc::new(ActiveActivity {
            context: context.into(),
            invocations: Arc::default(),
            related: tokio::sync::Mutex::default(),
        }));
        Ok(())
    }

    pub(crate) async fn deactivate(&self) {
        *self.active.write().await = None;
    }

    pub(crate) async fn reconcile(&self, activity: ActivityId) -> Result<(), String> {
        if let Some(publisher) = &self.publisher {
            Box::pin(publisher.reconcile(activity))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) async fn access_current(&self) -> Result<ActivityAccess, String> {
        let guard = Arc::clone(&self.active).read_owned().await;
        let activity = guard
            .as_ref()
            .ok_or_else(|| "no activity is active".to_owned())?;
        let context = activity.context.clone();
        let activity = Arc::clone(activity);
        Ok(ActivityAccess {
            guard,
            context,
            activity,
        })
    }
}

#[derive(Debug)]
struct ActiveActivity {
    context: ActivityContext,
    invocations: Arc<tokio::sync::Mutex<BTreeMap<String, usize>>>,
    related: tokio::sync::Mutex<BTreeSet<String>>,
}

#[derive(Debug)]
pub(super) struct ActivityAccess {
    guard: tokio::sync::OwnedRwLockReadGuard<Option<Arc<ActiveActivity>>>,
    context: ActivityContext,
    activity: Arc<ActiveActivity>,
}

impl ActivityAccess {
    pub(super) fn task_id(&self, requested: Option<i64>) -> Result<i64, String> {
        if let Some(task) = requested {
            return Ok(task);
        }
        match self.context.activity {
            ActivityId::Task(task) => Ok(task),
            _ => Err(
                "provide the task ID from task_status; only the building role has an implicit \
                 active task"
                    .to_owned(),
            ),
        }
    }

    pub(super) async fn require_subject(&self, request: &SubjectRequest) -> Result<(), String> {
        let context = self.context();
        let matches = match &context.source {
            // Chat senders are authenticated and mapped to a DID before ChatContext exists, so
            // they may address any subject in this member's already-fixed repository.
            Source::Chat(_) => true,
            Source::Subject(Subject::Issue(id)) if request.kind == SubjectKind::Issue => {
                request.subject == id.to_string()
            }
            Source::Subject(Subject::Patch(id)) if request.kind == SubjectKind::Patch => {
                request.subject == id.to_string()
            }
            Source::Subject(_) => false,
        };
        let reference = format!(
            "{}:{}",
            match request.kind {
                SubjectKind::Issue => "issue",
                SubjectKind::Patch => "patch",
            },
            request.subject
        );
        if !matches && !self.activity.related.lock().await.contains(&reference) {
            return Err(format!(
                "activity from {} cannot write to {reference}: no task relationship was verified; \
                 use get_subject to inspect it and link the requested task in the originating \
                 discussion",
                context.source
            ));
        }
        Ok(())
    }

    pub(super) async fn relate(&self, subject: Subject) {
        let _previous = self
            .activity
            .related
            .lock()
            .await
            .insert(subject.to_string());
    }

    pub(super) fn context(&self) -> &ActivityContext {
        let _keep_active = &self.guard;
        &self.context
    }

    pub(super) fn invocations(&self) -> &Arc<tokio::sync::Mutex<BTreeMap<String, usize>>> {
        &self.activity.invocations
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("MCP server I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("MCP server task failed: {0}")]
    Task(String),
    #[error("invalid MCP server endpoint: {0}")]
    Endpoint(#[from] url::ParseError),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ServiceError {
    #[error("MCP service task failed: {0}")]
    Task(String),
    #[error("invalid Radicle identifier '{0}'")]
    InvalidId(String),
    /// The request itself is malformed or outside a documented limit.
    #[error("{0}")]
    Request(String),
    #[error("object or entry '{0}' was not found")]
    NotFound(String),
    #[error("timeline cursor '{0}' was not found")]
    InvalidCursor(String),
    #[error("the requested operation requires a different subject kind")]
    SubjectKind,
    #[error("thread kind, subject kind, and entity do not form a valid thread")]
    InvalidThread,
    #[error("diff exceeds the response size limit")]
    DiffLimit,
    #[error("embed exceeds the response size limit")]
    EmbedLimit,
    #[error("failed to open Radicle repository: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    #[error("failed to read Radicle collaborative objects: {0}")]
    Store(#[from] radicle::cob::store::Error),
    #[error("failed to open collaborative object operation stream: {0}")]
    Stream(#[from] radicle::cob::stream::error::Stream),
    #[error("failed to validate a timeline cursor: {0}")]
    Git(#[from] radicle::git::raw::Error),
    #[error("failed to read collaborative object operation: {0}")]
    Operation(#[source] radicle::cob::stream::error::Ops),
    #[error("failed to encode an MCP response: {0}")]
    Encoding(#[source] serde_json::Error),
    /// A collaborative object title is invalid.
    #[error("invalid title: {0}")]
    Title(#[from] radicle::cob::TitleError),
    /// A Radicle object identifier is malformed.
    #[error("invalid object identifier: {0}")]
    ObjectId(#[from] radicle::cob::object::ParseObjectId),
    /// Publishing an authorized Radicle write failed.
    #[error(transparent)]
    Publish(#[from] crate::agent::PublishError),
    /// Radicle reserves issue assignment and labels to repository delegates.
    #[error(
        "Radicle requires a repository delegate to change issue {0}; ask a delegate to apply this \
         change"
    )]
    IssueDelegate(&'static str),
    /// Loading the policy used to verify task references failed.
    #[error("cannot verify task relationships: {0}")]
    Manifest(#[from] crate::manifest::Error),
}

impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        Self::Request(message)
    }
}
