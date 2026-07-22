//! Run-scoped, read-only Radicle context exposed over MCP.

use std::{net::Ipv4Addr, str::FromStr as _};

use radicle::{
    cob::{
        EntryId, ObjectId, Op,
        issue::{Action as IssueAction, IssueStream, Issues, TYPENAME as ISSUE_TYPENAME},
        patch::{Action as PatchAction, PatchStream, Patches, TYPENAME as PATCH_TYPENAME},
        store::access::ReadOnly,
        stream::CobStream,
    },
    identity::RepoId,
    storage::{ReadStorage as _, git::Storage},
};
use rand::distr::{Alphanumeric, SampleString as _};
use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig,
        streamable_http_server::{
            session::local::LocalSessionManager, tower::StreamableHttpService,
        },
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::agent::McpEndpoint;

const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 200;

macro_rules! page_reader {
    ($name:ident, $action:ty, $kind:expr, $stream:ident, $typename:expr) => {
        fn $name(
            &self,
            subject: &SubjectRequest,
            cursor: Option<&str>,
            limit: Option<u32>,
            predicate: impl Fn(&Op<$action>) -> bool,
        ) -> Result<TimelinePage, ContextError> {
            let (repository, id) = self.operation_source(subject, $kind)?;
            operation_page(
                &repository,
                id,
                &$typename,
                &$stream::init(id, &repository),
                cursor,
                limit,
                predicate,
            )
        }
    };
}

macro_rules! entry_reader {
    ($name:ident, $kind:expr, $stream:ident) => {
        fn $name(
            &self,
            subject: &SubjectRequest,
            entry: EntryId,
            requested: String,
        ) -> Result<Value, ContextError> {
            let (repository, id) = self.operation_source(subject, $kind)?;
            find_operation($stream::init(id, &repository).all()?, entry, requested)
        }
    };
}

/// Embedded MCP server scoped to one Radicle profile and repository.
pub(crate) struct Server {
    endpoint: McpEndpoint,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextServer")
            .finish_non_exhaustive()
    }
}

impl Server {
    pub(crate) async fn start(storage: Storage, repository: RepoId) -> Result<Self, Error> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let capability = Alphanumeric.sample_string(&mut rand::rng(), 48);
        let route = format!("/mcp/{capability}");
        let endpoint = McpEndpoint::new(Url::parse(&format!("http://{address}{route}"))?);
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.child_token();
        let task = tokio::spawn(Self::serve(
            listener,
            route,
            ContextService::new(storage, repository),
            server_cancellation,
        ));

        Ok(Self {
            endpoint,
            cancellation,
            task: Some(task),
        })
    }

    pub(crate) const fn endpoint(&self) -> &McpEndpoint {
        &self.endpoint
    }

    pub(crate) async fn stop(mut self) -> Result<(), Error> {
        self.shutdown().await
    }

    async fn serve(
        listener: tokio::net::TcpListener,
        route: String,
        context: ContextService,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let service: StreamableHttpService<ContextService, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(context.clone()),
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

#[derive(Debug, Clone)]
struct ContextService {
    storage: Storage,
    repository: RepoId,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ContextService {
    fn new(storage: Storage, repository: RepoId) -> Self {
        Self {
            storage,
            repository,
            tool_router: Self::tool_router(),
        }
    }

    /// Return the current materialized state of an issue or patch.
    #[tool(description = "Return the current state of a Radicle issue or patch")]
    async fn get_subject(
        &self,
        Parameters(request): Parameters<SubjectRequest>,
    ) -> Result<Json<Document>, String> {
        self.blocking(move |service| service.subject(request))
            .await
            .map(|data| Json(Document { data }))
            .map_err(|error| error.to_string())
    }

    /// Return one operation from an issue or patch timeline.
    #[tool(description = "Return one operation from a Radicle issue or patch timeline")]
    async fn get_entry(
        &self,
        Parameters(request): Parameters<EntryRequest>,
    ) -> Result<Json<Document>, String> {
        self.blocking(move |service| service.entry(request))
            .await
            .map(|data| Json(Document { data }))
            .map_err(|error| error.to_string())
    }

    /// Return a chronological page of operations from an issue or patch timeline.
    #[tool(description = "List a chronological page of issue or patch operations")]
    async fn list_timeline(
        &self,
        Parameters(request): Parameters<TimelineRequest>,
    ) -> Result<Json<TimelinePage>, String> {
        self.blocking(move |service| service.timeline(request))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    /// Return a page of operations belonging to an issue, review, or revision discussion.
    #[tool(description = "List operations in an issue, review, or revision discussion")]
    async fn get_thread(
        &self,
        Parameters(request): Parameters<EntityRequest>,
    ) -> Result<Json<TimelinePage>, String> {
        self.blocking(move |service| service.thread(request))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    /// Return a page of operations which define or modify a patch review.
    #[tool(description = "List operations defining or modifying a patch review")]
    async fn get_review(
        &self,
        Parameters(request): Parameters<PatchEntityRequest>,
    ) -> Result<Json<TimelinePage>, String> {
        self.blocking(move |service| service.patch_entity(request, PatchEntity::Review))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    /// Return a page of operations which define or refer to a patch revision.
    #[tool(description = "List operations defining or referring to a patch revision")]
    async fn get_revision(
        &self,
        Parameters(request): Parameters<PatchEntityRequest>,
    ) -> Result<Json<TimelinePage>, String> {
        self.blocking(move |service| service.patch_entity(request, PatchEntity::Revision))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ContextService {}

impl ContextService {
    entry_reader!(issue_entry, SubjectKind::Issue, IssueStream);

    entry_reader!(patch_entry, SubjectKind::Patch, PatchStream);

    page_reader!(
        issue_page,
        IssueAction,
        SubjectKind::Issue,
        IssueStream,
        ISSUE_TYPENAME
    );

    page_reader!(
        patch_page,
        PatchAction,
        SubjectKind::Patch,
        PatchStream,
        PATCH_TYPENAME
    );

    async fn blocking<T, F>(&self, operation: F) -> Result<T, ContextError>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Result<T, ContextError> + Send + 'static,
    {
        let service = self.clone();
        tokio::task::spawn_blocking(move || operation(service))
            .await
            .map_err(|error| ContextError::Task(error.to_string()))?
    }

    fn repository(&self) -> Result<radicle::storage::git::Repository, ContextError> {
        self.storage
            .repository(self.repository)
            .map_err(ContextError::Repository)
    }

    fn subject(&self, request: SubjectRequest) -> Result<Value, ContextError> {
        let document = match request.kind {
            SubjectKind::Issue => self.issue_subject(request),
            SubjectKind::Patch => self.patch_subject(request),
        }?;
        serialize(&document)
    }

    fn issue_subject(&self, request: SubjectRequest) -> Result<SubjectDocument, ContextError> {
        let (repository, id) = self.operation_source(&request, SubjectKind::Issue)?;
        let issue = Issues::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or_else(|| ContextError::NotFound(request.subject.clone()))?;
        Ok(SubjectDocument::issue(request.subject, &issue))
    }

    fn patch_subject(&self, request: SubjectRequest) -> Result<SubjectDocument, ContextError> {
        let (repository, id) = self.operation_source(&request, SubjectKind::Patch)?;
        let patch = Patches::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or_else(|| ContextError::NotFound(request.subject.clone()))?;
        Ok(SubjectDocument::patch(request.subject, &patch))
    }

    fn entry(&self, request: EntryRequest) -> Result<Value, ContextError> {
        let entry = parse_entry(&request.entry)?;
        match request.subject.kind {
            SubjectKind::Issue => self.issue_entry(&request.subject, entry, request.entry),
            SubjectKind::Patch => self.patch_entry(&request.subject, entry, request.entry),
        }
    }

    fn timeline(&self, request: TimelineRequest) -> Result<TimelinePage, ContextError> {
        match request.subject.kind {
            SubjectKind::Issue => self.issue_page(
                &request.subject,
                request.cursor.as_deref(),
                request.limit,
                |_| true,
            ),
            SubjectKind::Patch => self.patch_page(
                &request.subject,
                request.cursor.as_deref(),
                request.limit,
                |_| true,
            ),
        }
    }

    fn thread(&self, request: EntityRequest) -> Result<TimelinePage, ContextError> {
        match request.thread {
            ThreadKind::Issue => self.issue_thread(&request),
            ThreadKind::Review => self.review_thread(&request),
            ThreadKind::Revision => self.revision_thread(&request),
        }
    }

    fn issue_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ContextError> {
        if request.subject.kind != SubjectKind::Issue || request.entity.is_some() {
            return Err(ContextError::InvalidThread);
        }
        self.issue_page(
            &request.subject,
            request.cursor.as_deref(),
            request.limit,
            |operation| operation.actions.iter().any(issue_discussion_action),
        )
    }

    fn review_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ContextError> {
        self.patch_thread(request, review_discussion_operation)
    }

    fn revision_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ContextError> {
        self.patch_thread(request, revision_discussion_operation)
    }

    fn patch_thread(
        &self,
        request: &EntityRequest,
        predicate: impl Fn(&Op<PatchAction>, &str) -> bool,
    ) -> Result<TimelinePage, ContextError> {
        let id = request
            .entity
            .as_deref()
            .ok_or(ContextError::InvalidThread)?;
        self.patch_page(
            &request.subject,
            request.cursor.as_deref(),
            request.limit,
            |operation| predicate(operation, id),
        )
    }

    fn patch_entity(
        &self,
        request: PatchEntityRequest,
        entity: PatchEntity,
    ) -> Result<TimelinePage, ContextError> {
        let _id = parse_entry(&request.entity)?;
        let page = self.patch_page(
            &request.subject,
            request.cursor.as_deref(),
            request.limit,
            |operation| match entity {
                PatchEntity::Review => review_operation(operation, &request.entity),
                PatchEntity::Revision => revision_operation(operation, &request.entity),
            },
        )?;
        if request.cursor.is_none() && page.entries.is_empty() {
            Err(ContextError::NotFound(request.entity))
        } else {
            Ok(page)
        }
    }

    fn operation_source(
        &self,
        subject: &SubjectRequest,
        expected: SubjectKind,
    ) -> Result<(radicle::storage::git::Repository, ObjectId), ContextError> {
        if subject.kind != expected {
            return Err(ContextError::SubjectKind);
        }
        Ok((self.repository()?, parse_object(&subject.subject)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SubjectRequest {
    kind: SubjectKind,
    subject: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SubjectDocument {
    kind: SubjectKind,
    subject: String,
    title: String,
    description: String,
    state: String,
    author: String,
    assignees: Vec<String>,
    labels: Vec<String>,
    base: Option<String>,
    head: Option<String>,
}

impl SubjectDocument {
    fn issue(subject: String, issue: &radicle::cob::issue::Issue) -> Self {
        Self::new(
            SubjectCore {
                kind: SubjectKind::Issue,
                subject,
                title: issue.title(),
                description: issue.description(),
                state: issue.state().to_string(),
                author: issue.author().to_string(),
                commits: (None, None),
            },
            issue.assignees(),
            issue.labels(),
        )
    }

    fn patch(subject: String, patch: &radicle::cob::patch::Patch) -> Self {
        Self::new(
            SubjectCore {
                kind: SubjectKind::Patch,
                subject,
                title: patch.title(),
                description: patch.description(),
                state: patch.state().to_string(),
                author: patch.author().to_string(),
                commits: (Some(patch.base()), Some(patch.head())),
            },
            patch.assignees(),
            patch.labels(),
        )
    }

    fn new<A, L>(
        core: SubjectCore<'_>,
        assignees: impl IntoIterator<Item = A>,
        labels: impl IntoIterator<Item = L>,
    ) -> Self
    where
        A: ToString,
        L: ToString,
    {
        Self {
            kind: core.kind,
            subject: core.subject,
            title: core.title.to_owned(),
            description: core.description.to_owned(),
            state: core.state,
            author: core.author,
            assignees: assignees
                .into_iter()
                .map(|value| value.to_string())
                .collect(),
            labels: labels.into_iter().map(|value| value.to_string()).collect(),
            base: core.commits.0.map(ToString::to_string),
            head: core.commits.1.map(ToString::to_string),
        }
    }
}

struct SubjectCore<'a> {
    kind: SubjectKind,
    subject: String,
    title: &'a str,
    description: &'a str,
    state: String,
    author: String,
    commits: (Option<&'a radicle::git::Oid>, Option<&'a radicle::git::Oid>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SubjectKind {
    Issue,
    Patch,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EntryRequest {
    #[serde(flatten)]
    subject: SubjectRequest,
    entry: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TimelineRequest {
    #[serde(flatten)]
    subject: SubjectRequest,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TimelinePage {
    entries: Vec<Value>,
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Document {
    data: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EntityRequest {
    #[serde(flatten)]
    subject: SubjectRequest,
    thread: ThreadKind,
    entity: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ThreadKind {
    Issue,
    Review,
    Revision,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PatchEntityRequest {
    #[serde(flatten)]
    subject: SubjectRequest,
    entity: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum PatchEntity {
    Review,
    Revision,
}

fn find_operation<A: Serialize>(
    operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
    entry: EntryId,
    requested: String,
) -> Result<Value, ContextError> {
    for operation in operations {
        let operation = operation.map_err(ContextError::Operation)?;
        if operation.id == entry {
            return serialize(&operation);
        }
    }
    Err(ContextError::NotFound(requested))
}

fn operation_page<S>(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    stream: &S,
    cursor: Option<&str>,
    limit: Option<u32>,
    predicate: impl Fn(&Op<S::Action>) -> bool,
) -> Result<TimelinePage, ContextError>
where
    S: CobStream<IterError = radicle::cob::stream::error::Ops>,
    S::Action: Serialize,
{
    let cursor = parse_cursor(cursor)?;
    validate_cursor(repository, object, typename, cursor)?;
    let operations = resume_stream(stream, cursor)?;
    paginate_operations(operations, cursor, limit, predicate)
}

fn resume_stream<S>(stream: &S, cursor: Option<EntryId>) -> Result<S::Iter, ContextError>
where
    S: CobStream<IterError = radicle::cob::stream::error::Ops>,
{
    match cursor {
        Some(cursor) => stream.since(cursor).map_err(ContextError::from),
        None => stream.all().map_err(ContextError::from),
    }
}

fn paginate_operations<A: Serialize>(
    operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
    cursor: Option<EntryId>,
    limit: Option<u32>,
    predicate: impl Fn(&Op<A>) -> bool,
) -> Result<TimelinePage, ContextError> {
    let mut operations = operations.into_iter();
    if let Some(cursor) = cursor {
        match operations.next() {
            Some(Ok(operation)) if operation.id == cursor && predicate(&operation) => {}
            _ => return Err(ContextError::InvalidCursor(cursor.to_string())),
        }
    }
    let limit = limit
        .map_or(DEFAULT_PAGE_SIZE, |limit| limit as usize)
        .clamp(1, MAX_PAGE_SIZE);
    let mut entries = Vec::with_capacity(limit);
    let mut last = None;
    for operation in operations {
        let operation = operation.map_err(ContextError::Operation)?;
        if !predicate(&operation) {
            continue;
        }
        if entries.len() == limit {
            return Ok(TimelinePage {
                entries,
                next_cursor: last.map(|id: EntryId| id.to_string()),
            });
        }
        last = Some(operation.id);
        entries.push(serialize(&operation)?);
    }

    Ok(TimelinePage {
        entries,
        next_cursor: None,
    })
}

fn parse_cursor(cursor: Option<&str>) -> Result<Option<EntryId>, ContextError> {
    cursor
        .map(|cursor| {
            EntryId::from_str(cursor)
                .map_err(|_error| ContextError::InvalidCursor(cursor.to_owned()))
        })
        .transpose()
}

fn validate_cursor(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    cursor: Option<EntryId>,
) -> Result<(), ContextError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor_reachable(repository, object, typename, cursor)? {
        Ok(())
    } else {
        Err(ContextError::InvalidCursor(cursor.to_string()))
    }
}

fn cursor_reachable(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    cursor: EntryId,
) -> Result<bool, ContextError> {
    let refs = radicle::storage::refs::cobs(typename, &object);
    let reachable = repository
        .backend
        .references_glob(refs.as_str())?
        .map(|reference| reference_reaches(repository, reference, cursor))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(reachable.into_iter().any(|reachable| reachable))
}

fn reference_reaches(
    repository: &radicle::storage::git::Repository,
    reference: Result<radicle::git::raw::Reference<'_>, radicle::git::raw::Error>,
    cursor: EntryId,
) -> Result<bool, ContextError> {
    let reference = reference?;
    let Some(tip) = reference.resolve()?.target().map(radicle::git::Oid::from) else {
        return Ok(false);
    };
    if tip == cursor {
        return Ok(true);
    }
    repository
        .backend
        .graph_descendant_of(tip.into(), cursor.into())
        .map_err(ContextError::from)
}

fn issue_discussion_action(action: &IssueAction) -> bool {
    matches!(
        action,
        IssueAction::Comment { .. }
            | IssueAction::CommentEdit { .. }
            | IssueAction::CommentRedact { .. }
            | IssueAction::CommentReact { .. }
    )
}

fn review_discussion_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    operation.actions.iter().any(|action| match action {
        PatchAction::ReviewComment { review, .. }
        | PatchAction::ReviewCommentEdit { review, .. }
        | PatchAction::ReviewCommentRedact { review, .. }
        | PatchAction::ReviewCommentReact { review, .. }
        | PatchAction::ReviewCommentResolve { review, .. }
        | PatchAction::ReviewCommentUnresolve { review, .. } => review.to_string() == id,
        _ => false,
    })
}

fn revision_discussion_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    operation.actions.iter().any(|action| match action {
        PatchAction::RevisionComment { revision, .. }
        | PatchAction::RevisionCommentEdit { revision, .. }
        | PatchAction::RevisionCommentRedact { revision, .. }
        | PatchAction::RevisionCommentReact { revision, .. } => revision.to_string() == id,
        _ => false,
    })
}

fn review_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    patch_entity_operation(operation, id, PatchEntity::Review)
}

fn revision_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    patch_entity_operation(operation, id, PatchEntity::Revision)
}

fn patch_entity_operation(operation: &Op<PatchAction>, id: &str, entity: PatchEntity) -> bool {
    operation.actions.iter().any(|action| match entity {
        PatchEntity::Review => match action {
            PatchAction::Review { .. } => operation.id.to_string() == id,
            PatchAction::ReviewRedact { review }
            | PatchAction::ReviewReact { review, .. }
            | PatchAction::ReviewComment { review, .. }
            | PatchAction::ReviewCommentEdit { review, .. }
            | PatchAction::ReviewCommentRedact { review, .. }
            | PatchAction::ReviewCommentReact { review, .. }
            | PatchAction::ReviewCommentResolve { review, .. }
            | PatchAction::ReviewCommentUnresolve { review, .. } => review.to_string() == id,
            PatchAction::ReviewEdit(edit) => edit.review_id().to_string() == id,
            _ => false,
        },
        PatchEntity::Revision => revision_action(operation.id, action, id),
    })
}

fn revision_action(operation: EntryId, action: &PatchAction, id: &str) -> bool {
    match action {
        PatchAction::Revision { .. } => operation.to_string() == id,
        PatchAction::Review { revision, .. }
        | PatchAction::Merge { revision, .. }
        | PatchAction::RevisionEdit { revision, .. }
        | PatchAction::RevisionRedact { revision }
        | PatchAction::RevisionReact { revision, .. }
        | PatchAction::RevisionComment { revision, .. }
        | PatchAction::RevisionCommentEdit { revision, .. }
        | PatchAction::RevisionCommentRedact { revision, .. }
        | PatchAction::RevisionCommentReact { revision, .. } => revision.to_string() == id,
        _ => false,
    }
}

fn parse_object(value: &str) -> Result<ObjectId, ContextError> {
    ObjectId::from_str(value).map_err(|_error| ContextError::InvalidId(value.to_owned()))
}

fn parse_entry(value: &str) -> Result<EntryId, ContextError> {
    EntryId::from_str(value).map_err(|_error| ContextError::InvalidId(value.to_owned()))
}

fn serialize(value: &impl Serialize) -> Result<Value, ContextError> {
    serde_json::to_value(value).map_err(ContextError::Encoding)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("context server I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("context server task failed: {0}")]
    Task(String),
    #[error("invalid context server endpoint: {0}")]
    Endpoint(#[from] url::ParseError),
}

#[derive(Debug, thiserror::Error)]
enum ContextError {
    #[error("context task failed: {0}")]
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
    #[error("failed to encode Radicle context: {0}")]
    Encoding(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        cob::{EntryId, Manifest, Op, Timestamp, Version, patch::TYPENAME},
        crypto::PublicKey,
        git::Oid,
    };
    #[cfg(not(miri))]
    use radicle::{
        crypto::Seed,
        identity::RepoId,
        node::Alias,
        profile::{Home, Profile},
    };

    #[cfg(not(miri))]
    use super::Server;
    use super::{ContextService, PatchAction, paginate_operations, revision_action};

    #[cfg(not(miri))]
    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const AUTHOR: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[test]
    fn exposes_the_complete_context_tool_set() {
        let tools = ContextService::tool_router();
        let mut names = tools
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        names.sort_unstable();

        assert_eq!(
            names,
            [
                "get_entry",
                "get_review",
                "get_revision",
                "get_subject",
                "get_thread",
                "list_timeline",
            ]
        );
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn starts_on_a_loopback_capability_url_and_stops() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = Home::new(root.path()).expect("Radicle home should be created");
        let profile = Profile::init(home, Alias::new("context-test"), None, Seed::generate())
            .expect("Radicle profile should be initialized");
        let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
        let server = Server::start(profile.storage.clone(), repository)
            .await
            .expect("context server should start");

        assert_eq!(server.endpoint().url().host_str(), Some("127.0.0.1"));
        assert!(server.endpoint().url().path().starts_with("/mcp/"));
        server.stop().await.expect("context server should stop");
    }

    #[test]
    fn paginates_context_with_stable_operation_cursors() {
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
}
