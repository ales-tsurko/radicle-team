//! Loopback HTTP transport for the MCP endpoint.

use std::{collections::BTreeMap, net::Ipv4Addr, sync::Arc};

use radicle::{identity::RepoId, storage::git::Storage};
use rand::distr::{Alphanumeric, SampleString as _};
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    agent::{Context as AgentContext, McpEndpoint, Publisher},
    manifest::McpAllowlist,
    mcp::{
        protocol::{SubjectKind, SubjectRequest},
        security::SecurityReporting,
        tools::ToolService,
    },
    watcher::Subject,
};

/// Embedded MCP server scoped to one Radicle profile and repository.
pub(crate) struct Server {
    endpoint: McpEndpoint,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    delivery: DeliveryScope,
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
    ) -> Result<Self, Error> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let capability = Alphanumeric.sample_string(&mut rand::rng(), 48);
        let route = format!("/mcp/{capability}");
        let endpoint = McpEndpoint::new(Url::parse(&format!("http://{address}{route}"))?);
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.child_token();
        let delivery = DeliveryScope::new(publisher.clone());
        let security = SecurityReporting::new(&publisher);
        let task = tokio::spawn(Self::serve(
            listener,
            route,
            ToolService::new(
                storage,
                repository,
                delivery.clone(),
                publisher,
                allowlist,
                security,
            ),
            server_cancellation,
        ));

        Ok(Self {
            endpoint,
            cancellation,
            task: Some(task),
            delivery,
        })
    }

    pub(crate) const fn endpoint(&self) -> &McpEndpoint {
        &self.endpoint
    }

    pub(crate) fn delivery_scope(&self) -> DeliveryScope {
        self.delivery.clone()
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

/// Activates MCP access for exactly one watcher delivery.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeliveryScope {
    active: Arc<tokio::sync::RwLock<Option<Arc<ActiveDelivery>>>>,
    publisher: Option<Publisher>,
}

impl DeliveryScope {
    pub(super) fn new(publisher: Publisher) -> Self {
        Self {
            active: Arc::default(),
            publisher: Some(publisher),
        }
    }

    pub(crate) async fn activate(&self, context: AgentContext) -> Result<(), String> {
        let mut active = self.active.write().await;
        if active.is_some() {
            return Err("another delivery is already active".to_owned());
        }
        *active = Some(Arc::new(ActiveDelivery {
            context,
            invocations: Arc::default(),
        }));
        Ok(())
    }

    pub(crate) async fn deactivate(&self) {
        *self.active.write().await = None;
    }

    pub(crate) async fn reconcile(&self, delivery: crate::agent::DeliveryId) -> Result<(), String> {
        if let Some(publisher) = &self.publisher {
            Box::pin(publisher.reconcile(delivery)).await?;
        }
        Ok(())
    }

    pub(super) async fn access(&self, request: &SubjectRequest) -> Result<DeliveryAccess, String> {
        let access = self.access_current().await?;
        let context = access.context();
        let matches = match (request.kind, context.subject) {
            (SubjectKind::Issue, Subject::Issue(id)) | (SubjectKind::Patch, Subject::Patch(id)) => {
                request.subject == id.to_string()
            }
            _ => false,
        };
        if !matches {
            return Err("subject is outside the active delivery".to_owned());
        }
        Ok(access)
    }

    pub(super) async fn access_current(&self) -> Result<DeliveryAccess, String> {
        let guard = Arc::clone(&self.active).read_owned().await;
        let delivery = guard
            .as_ref()
            .ok_or_else(|| "no delivery is active".to_owned())?;
        let context = delivery.context.clone();
        let delivery = Arc::clone(delivery);
        Ok(DeliveryAccess {
            guard,
            context,
            delivery,
        })
    }
}

#[derive(Debug)]
struct ActiveDelivery {
    context: AgentContext,
    invocations: Arc<tokio::sync::Mutex<BTreeMap<String, usize>>>,
}

#[derive(Debug)]
pub(super) struct DeliveryAccess {
    guard: tokio::sync::OwnedRwLockReadGuard<Option<Arc<ActiveDelivery>>>,
    context: AgentContext,
    delivery: Arc<ActiveDelivery>,
}

impl DeliveryAccess {
    pub(super) fn context(&self) -> &AgentContext {
        let _keep_active = &self.guard;
        &self.context
    }

    pub(super) fn invocations(&self) -> &Arc<tokio::sync::Mutex<BTreeMap<String, usize>>> {
        &self.delivery.invocations
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
}
