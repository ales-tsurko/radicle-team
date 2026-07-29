//! MCP tool registration and request handlers.

use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use radicle::{identity::RepoId, storage::git::Storage};
use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use serde::Serialize;

use crate::{
    agent::Publisher,
    manifest::McpAllowlist,
    mcp::{
        protocol::{
            CommentIssueRequest, CommentOperation, CommentWriteRequest, CreateIssueRequest,
            CreatePatchRequest, DiffRequest, Document, EditIssueRequest, EditPatchRequest,
            EmbedReadRequest, EntityRequest, EntryRequest, IssuePage, IssueRequest,
            ListIssuesRequest, MergePatchRequest, PatchEntity, PatchEntityRequest,
            ReactionWriteRequest, ResolveCommentRequest, ReviewWriteRequest, SecurityIssueRequest,
            SecurityIssueResult, SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind,
            SubjectRequest, TimelinePage, TimelineRequest, UpdatePatchRequest,
        },
        read::serialize,
        security::SecurityReporting,
        server::{DeliveryAccess, DeliveryScope, ServiceError},
    },
};

#[derive(Debug, Clone)]
pub(crate) struct ToolService {
    pub(super) storage: Storage,
    pub(super) repository: RepoId,
    tool_router: ToolRouter<Self>,
    pub(super) delivery: DeliveryScope,
    pub(super) publisher: Publisher,
    security: SecurityReporting,
    pub(super) mutation_repository: Option<RepoId>,
    pub(super) mutation_delivery: Option<crate::agent::DeliveryId>,
    pub(super) mutation_invocations: Option<Arc<tokio::sync::Mutex<BTreeMap<String, usize>>>>,
}

#[tool_router(router = tool_router)]
impl ToolService {
    /// Returns names registered by the MCP router in stable order.
    pub(crate) fn registered_tools() -> &'static [String] {
        static TOOLS: OnceLock<Vec<String>> = OnceLock::new();

        TOOLS.get_or_init(|| {
            let mut tools = Self::tool_router()
                .list_all()
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect::<Vec<_>>();
            tools.sort_unstable();
            tools
        })
    }

    pub(super) fn new(
        storage: Storage,
        repository: RepoId,
        delivery: DeliveryScope,
        publisher: Publisher,
        allowlist: McpAllowlist,
        security: SecurityReporting,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        for method in Self::registered_tools() {
            if method != "report_security_issue" && !allowlist.allows(method) {
                let _disabled = tool_router.disable_route(method.clone());
            }
        }
        Self {
            storage,
            repository,
            tool_router,
            delivery,
            publisher,
            security,
            mutation_repository: None,
            mutation_delivery: None,
            mutation_invocations: None,
        }
    }

    #[tool(description = "List or search issues in the active repository")]
    async fn list_issues(
        &self,
        Parameters(request): Parameters<ListIssuesRequest>,
    ) -> Result<Json<IssuePage>, String> {
        self.publisher.authorize("list_issues").await?;
        let _active = self.delivery.access_current().await?;
        self.blocking(move |service| service.issues(request))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Return an issue from the active repository")]
    async fn get_issue(
        &self,
        Parameters(request): Parameters<IssueRequest>,
    ) -> Result<Json<Document>, String> {
        self.publisher.authorize("get_issue").await?;
        let _active = self.delivery.access_current().await?;
        self.blocking(move |service| service.issue(request))
            .await
            .map(|data| Json(Document { data }))
            .map_err(|error| error.to_string())
    }

    pub(super) fn for_delivery(&self, access: &DeliveryAccess) -> Self {
        let mut service = self.clone();
        service.mutation_repository = Some(access.context().repository);
        service.mutation_delivery = Some(access.context().delivery);
        service.mutation_invocations = Some(Arc::clone(access.invocations()));
        service
    }

    pub(super) fn for_repository(&self, repository: RepoId, publisher: Publisher) -> Self {
        let mut service = self.clone();
        service.repository = repository;
        service.publisher = publisher;
        service
    }

    async fn document<T, F>(&self, operation: F) -> Result<Json<Document>, String>
    where
        T: Serialize + Send + 'static,
        F: FnOnce(Self) -> Result<T, ServiceError> + Send + 'static,
    {
        self.blocking(operation)
            .await
            .and_then(|value| serialize(&value))
            .map(|data| Json(Document { data }))
            .map_err(|error| error.to_string())
    }

    /// Return the current materialized state of an issue or patch.
    #[tool(description = "Return the current state of a Radicle issue or patch")]
    async fn get_subject(
        &self,
        Parameters(request): Parameters<SubjectRequest>,
    ) -> Result<Json<Document>, String> {
        self.publisher.authorize("get_subject").await?;
        let _active = self.delivery.access(&request).await?;
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
        self.publisher.authorize("get_entry").await?;
        let _active = self.delivery.access(&request.subject).await?;
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
        self.publisher.authorize("list_timeline").await?;
        let _active = self.delivery.access(&request.subject).await?;
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
        self.publisher.authorize("get_thread").await?;
        let _active = self.delivery.access(&request.subject).await?;
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
        self.publisher.authorize("get_review").await?;
        let _active = self.delivery.access(&request.subject).await?;
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
        self.publisher.authorize("get_revision").await?;
        let _active = self.delivery.access(&request.subject).await?;
        self.blocking(move |service| service.patch_entity(request, PatchEntity::Revision))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Return a structured diff for a patch revision")]
    async fn get_diff(
        &self,
        Parameters(request): Parameters<DiffRequest>,
    ) -> Result<Json<Document>, String> {
        self.publisher.authorize("get_diff").await?;
        let _active = self.delivery.access(&request.subject).await?;
        self.document(move |service| service.diff(request)).await
    }

    /// Return base64 content for a Git-backed comment embed.
    #[tool(description = "Return the content of a Git-backed comment embed")]
    async fn get_embed(
        &self,
        Parameters(request): Parameters<EmbedReadRequest>,
    ) -> Result<Json<Document>, String> {
        self.publisher.authorize("get_embed").await?;
        let _active = self.delivery.access(&request.subject).await?;
        self.document(move |service| service.embed(request)).await
    }

    #[tool(description = "Create a comment on the active issue, patch revision, or review")]
    async fn create_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .write_comment(request, CommentOperation::Create)
            .await
    }

    #[tool(description = "Edit a comment on the active issue, patch revision, or review")]
    async fn edit_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .write_comment(request, CommentOperation::Edit)
            .await
    }

    #[tool(description = "Redact a comment on the active issue, patch revision, or review")]
    async fn redact_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .write_comment(request, CommentOperation::Redact)
            .await
    }

    #[tool(description = "Resolve or reopen a review comment")]
    async fn resolve_comment(
        &self,
        Parameters(request): Parameters<ResolveCommentRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .resolve_review_comment(request)
            .await
    }

    /// Add or remove an emoji reaction.
    #[tool(description = "Add or remove a reaction on an issue, revision, review, or comment")]
    async fn set_reaction(
        &self,
        Parameters(request): Parameters<ReactionWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active).reaction_write(request).await
    }

    #[tool(description = "Create an issue in the active repository")]
    async fn create_issue(
        &self,
        Parameters(request): Parameters<CreateIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access_current().await?;
        self.for_delivery(&active).create_issue_write(request).await
    }

    #[tool(description = "Comment on an issue in the active repository")]
    async fn comment_issue(
        &self,
        Parameters(request): Parameters<CommentIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access_current().await?;
        self.for_delivery(&active)
            .comment_issue_write(request)
            .await
    }

    #[tool(description = "Encrypt a report into the private security repository")]
    async fn report_security_issue(
        &self,
        Parameters(request): Parameters<SecurityIssueRequest>,
    ) -> Result<Json<SecurityIssueResult>, String> {
        let active = self.delivery.access_current().await?;
        let service = self.for_delivery(&active);
        self.security.report(&service, request).await.map(Json)
    }

    #[tool(description = "Edit an active issue title or description")]
    async fn edit_issue(
        &self,
        Parameters(request): Parameters<EditIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active).edit_issue_write(request).await
    }

    #[tool(description = "Open or close the active issue")]
    async fn set_issue_state(
        &self,
        Parameters(request): Parameters<SetStateRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_state_write(request, SubjectKind::Issue)
            .await
    }

    #[tool(description = "Replace the active issue labels")]
    async fn set_issue_labels(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_strings_write(request, SubjectKind::Issue, StringUpdate::Labels)
            .await
    }

    #[tool(description = "Replace the active issue assignees")]
    async fn set_issue_assignees(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_strings_write(request, SubjectKind::Issue, StringUpdate::Assignees)
            .await
    }

    #[tool(description = "Create a patch from commits in the active workspace")]
    async fn create_patch(
        &self,
        Parameters(request): Parameters<CreatePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access_current().await?;
        self.for_delivery(&active)
            .create_patch_write(request, active.context().workspace.clone())
            .await
    }

    #[tool(description = "Add a revision from commits in the active workspace")]
    async fn update_patch(
        &self,
        Parameters(request): Parameters<UpdatePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .update_patch_write(request, active.context().workspace.clone())
            .await
    }

    #[tool(description = "Edit active patch metadata")]
    async fn edit_patch(
        &self,
        Parameters(request): Parameters<EditPatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active).edit_patch_write(request).await
    }

    #[tool(description = "Set the active patch lifecycle")]
    async fn set_patch_state(
        &self,
        Parameters(request): Parameters<SetStateRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_state_write(request, SubjectKind::Patch)
            .await
    }

    #[tool(description = "Replace the active patch labels")]
    async fn set_patch_labels(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_strings_write(request, SubjectKind::Patch, StringUpdate::Labels)
            .await
    }

    #[tool(description = "Replace the active patch assignees")]
    async fn set_patch_assignees(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .set_strings_write(request, SubjectKind::Patch, StringUpdate::Assignees)
            .await
    }

    #[tool(description = "Create a review for an active patch revision")]
    async fn create_review(
        &self,
        Parameters(request): Parameters<ReviewWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active)
            .review_write(request, false)
            .await
    }

    #[tool(description = "Edit an existing review on the active patch")]
    async fn edit_review(
        &self,
        Parameters(request): Parameters<ReviewWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active).review_write(request, true).await
    }

    #[tool(description = "Record a merge attestation for the active patch")]
    async fn merge_patch(
        &self,
        Parameters(request): Parameters<MergePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.delivery.access(&request.subject).await?;
        self.for_delivery(&active).merge_patch_write(request).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ToolService {}
