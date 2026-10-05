//! MCP tool registration and request handlers.

use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use radicle::{identity::RepoId, storage::git::Storage};
use rmcp::{
    ErrorData, Json, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Serialize;

use crate::{
    agent::{ActivityId, Publisher, Role, Source},
    chat::State as ChatState,
    manifest::McpAllowlist,
    mcp::{
        protocol::{
            AddTaskRequest, ChatReplyRecord, CommentIssueRequest, CommentOperation,
            CommentWriteRequest, ContextRequest, ContextUpdateRequest, CreateIssueRequest,
            CreatePatchRequest, DiffRequest, Document, EditIssueRequest, EditPatchRequest,
            EmbedReadRequest, EntityRequest, EntryRequest, HandoffRequest, IssuePage, IssueRequest,
            ListIssuesRequest, MergePatchRequest, NoteRequest, PatchEntity, PatchEntityRequest,
            ReactionWriteRequest, ReplyChatRequest, ResolveCommentRequest, ReviewWriteRequest,
            SecurityIssueRequest, SecurityIssueResult, SetStateRequest, SetStringsRequest,
            StringUpdate, SubjectKind, SubjectRequest, TaskAction, TaskRecord, TaskRequest,
            TaskStatus, TaskSummary, TimelinePage, TimelineRequest, UpdatePatchRequest,
        },
        read::serialize,
        security::SecurityReporting,
        server::{ActivityAccess, ActivityScope, ServiceError},
    },
    queue::{ArtifactRole, TaskQueue},
};

// Tools which coordinate the member's roles rather than write to the repository.
const QUEUE_TOOLS: [&str; 5] = ["add_task", "cancel", "handoff_task", "note", "task_status"];
// Tools which need commits that only the building role can make.
const BUILDING_TOOLS: [&str; 2] = ["create_patch", "update_patch"];
// Tool which only the chatting role can use, while handling an authenticated message.
const CHAT_TOOLS: [&str; 1] = ["reply_chat"];
// Tool which every member may always reach, whatever the manifest allows.
const CONTEXT_TOOLS: [&str; 2] = ["get_task_context", "update_task_context"];
const SECURITY_TOOL: &str = "report_security_issue";

impl Role {
    // Returns whether this role of a member can make any use of `method`.
    pub(super) fn allows(self, method: &str) -> bool {
        match self {
            Self::Talker => !BUILDING_TOOLS.contains(&method) && !CHAT_TOOLS.contains(&method),
            Self::Chatter => !BUILDING_TOOLS.contains(&method),
            Self::Coder => !QUEUE_TOOLS.contains(&method) && !CHAT_TOOLS.contains(&method),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolService {
    pub(super) storage: Storage,
    pub(super) repository: RepoId,
    tool_router: ToolRouter<Self>,
    pub(super) scope: ActivityScope,
    pub(super) publisher: Publisher,
    security: SecurityReporting,
    pub(super) role: Role,
    pub(super) queue: TaskQueue,
    chat: ChatState,
    pub(super) creations: Arc<tokio::sync::Mutex<()>>,
    pub(super) mutation_repository: Option<RepoId>,
    pub(super) mutation_activity: Option<crate::agent::ActivityId>,
    pub(super) mutation_invocations: Option<Arc<tokio::sync::Mutex<BTreeMap<String, usize>>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolState {
    queue: TaskQueue,
    chat: ChatState,
}

impl ToolState {
    pub(crate) const fn new(queue: TaskQueue, chat: ChatState) -> Self {
        Self { queue, chat }
    }
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

    /// Returns whether the repository manifest has any say in `method`.
    ///
    /// These reach the member's own queue, or the security policy, and never ordinary repository
    /// state, so naming one in an allowlist is a mistake rather than a grant.
    pub(crate) fn is_unrestricted(method: &str) -> bool {
        QUEUE_TOOLS.contains(&method) || CONTEXT_TOOLS.contains(&method) || method == SECURITY_TOOL
    }

    pub(super) fn new(
        storage: Storage,
        repository: RepoId,
        scope: ActivityScope,
        publisher: Publisher,
        allowlist: McpAllowlist,
        role: Role,
        state: ToolState,
    ) -> Self {
        let ToolState { queue, chat } = state;
        let creations = Arc::clone(&queue.creations);
        let security = SecurityReporting::new(&publisher);
        let mut tool_router = Self::tool_router();
        for method in Self::registered_tools() {
            if !allowlist.allows(method) || !role.allows(method) {
                let _disabled = tool_router.disable_route(method.clone());
            }
        }
        Self {
            storage,
            repository,
            tool_router,
            scope,
            publisher,
            security,
            role,
            queue,
            chat,
            creations,
            mutation_repository: None,
            mutation_activity: None,
            mutation_invocations: None,
        }
    }

    #[tool(description = "List or search issues in the active repository")]
    async fn list_issues(
        &self,
        Parameters(request): Parameters<ListIssuesRequest>,
    ) -> Result<Json<IssuePage>, String> {
        self.authorize("list_issues").await?;
        let _active = self.active().await?;
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
        self.authorize("get_issue").await?;
        let _active = self.active().await?;
        self.blocking(move |service| service.issue(request))
            .await
            .map(|data| Json(Document { data }))
            .map_err(|error| error.to_string())
    }

    /// Confirms the manifest still allows `method`, reporting failure as MCP tool output.
    async fn authorize(&self, method: &'static str) -> Result<(), String> {
        self.publisher
            .authorize(method)
            .await
            .map_err(|error| error.to_string())
    }

    /// Returns whether this server exposes `method` at all.
    #[cfg(test)]
    pub(super) fn offers(&self, method: &str) -> bool {
        self.tool_router.has_route(method)
    }

    pub(super) fn for_work(&self, access: &ActivityAccess) -> Self {
        let mut service = self.clone();
        service.mutation_repository = Some(access.context().repository);
        service.mutation_activity = Some(access.context().activity);
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
    pub(super) async fn get_subject(
        &self,
        Parameters(request): Parameters<SubjectRequest>,
    ) -> Result<Json<Document>, String> {
        self.authorize("get_subject").await?;
        let _active = self.active().await?;
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
        self.authorize("get_entry").await?;
        let _active = self.active().await?;
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
        self.authorize("list_timeline").await?;
        let _active = self.active().await?;
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
        self.authorize("get_thread").await?;
        let _active = self.active().await?;
        self.blocking(move |service| service.thread(request))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    /// Return a page of operations which define or modify a patch review.
    #[tool(
        description = "List operations defining or modifying a patch review, including the \
                       operation IDs and authors. Verify the review's actual author here; a \
                       requested reviewer or discussion order does not establish authorship."
    )]
    async fn get_review(
        &self,
        Parameters(request): Parameters<PatchEntityRequest>,
    ) -> Result<Json<TimelinePage>, String> {
        self.authorize("get_review").await?;
        let _active = self.active().await?;
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
        self.authorize("get_revision").await?;
        let _active = self.active().await?;
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
        self.authorize("get_diff").await?;
        let _active = self.active().await?;
        self.document(move |service| service.diff(request)).await
    }

    /// Return base64 content for a Git-backed comment embed.
    #[tool(description = "Return the content of a Git-backed comment embed")]
    async fn get_embed(
        &self,
        Parameters(request): Parameters<EmbedReadRequest>,
    ) -> Result<Json<Document>, String> {
        self.authorize("get_embed").await?;
        let _active = self.active().await?;
        self.document(move |service| service.embed(request)).await
    }

    #[tool(
        description = "Create a comment on the active issue, patch revision, or review. Keep \
                       patch discussion and review requests on the patch. An issue handoff needs \
                       only the patch ID and any new blocker or decision; do not repeat patch \
                       details or reviewer mentions."
    )]
    pub(super) async fn create_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.comment_boundary(&active, &request).await?;
        self.for_work(&active)
            .write_comment(request, CommentOperation::Create)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Edit a comment on the active issue, patch revision, or review")]
    async fn edit_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.comment_boundary(&active, &request).await?;
        self.for_work(&active)
            .write_comment(request, CommentOperation::Edit)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Redact a comment on the active issue, patch revision, or review")]
    async fn redact_comment(
        &self,
        Parameters(request): Parameters<CommentWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.comment_boundary(&active, &request).await?;
        self.for_work(&active)
            .write_comment(request, CommentOperation::Redact)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Resolve or reopen a review comment")]
    async fn resolve_comment(
        &self,
        Parameters(request): Parameters<ResolveCommentRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.verify_review_thread(&active, &request.subject, &request.review)
            .await?;
        self.for_work(&active)
            .resolve_review_comment(request)
            .await
            .map_err(|error| error.to_string())
    }

    /// Add or remove an emoji reaction.
    #[tool(
        description = "Add or remove a reaction on an issue, revision, review, or comment. Choose \
                       the emoji to fit your character, role, and the message being acknowledged."
    )]
    async fn set_reaction(
        &self,
        Parameters(request): Parameters<ReactionWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        if matches!(
            request.target,
            crate::mcp::protocol::CommentTargetKind::Review
        ) && let Some(review) = &request.entity
        {
            self.verify_review_thread(&active, &request.subject, review)
                .await?;
        }
        self.for_work(&active)
            .reaction_write(request)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Create an issue in the active repository")]
    pub(super) async fn create_issue(
        &self,
        Parameters(request): Parameters<CreateIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.active().await?;
        self.building_may_write(&active).await?;
        self.authorize("create_issue").await?;
        let _creation = self.creations.lock().await;
        let created = if let Some(id) = self.existing_issue(&request.title).await? {
            Document::reused(id, "Use the existing issue; add only new information")
        } else {
            self.for_work(&active)
                .create_issue_write(request)
                .await
                .map_err(|error| error.to_string())?
        };
        if let Some(issue) = crate::mcp::protocol::written_id(&created) {
            self.record_relation(
                &active,
                crate::watcher::Subject::Issue(issue),
                ArtifactRole::Produced,
            )
            .await?;
        }
        Ok(created)
    }

    #[tool(
        description = "Comment on an issue in the active repository. A patch-ready notification \
                       needs only its ID and any new blocker or decision. Keep implementation \
                       details, validation, and review requests on the patch; do not repeat \
                       reviewer mentions."
    )]
    async fn comment_issue(
        &self,
        Parameters(request): Parameters<CommentIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self
            .access_subject(&SubjectRequest {
                kind: SubjectKind::Issue,
                subject: request.issue.clone(),
            })
            .await?;
        self.for_work(&active)
            .comment_issue_write(request)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Encrypt a report into the private security repository")]
    async fn report_security_issue(
        &self,
        Parameters(request): Parameters<SecurityIssueRequest>,
    ) -> Result<Json<SecurityIssueResult>, String> {
        let active = self.active().await?;
        self.building_may_write(&active).await?;
        let service = self.for_work(&active);
        self.security.report(&service, request).await.map(Json)
    }

    #[tool(description = "Edit an active issue title or description")]
    async fn edit_issue(
        &self,
        Parameters(request): Parameters<EditIssueRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .edit_issue_write(request)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Open or close the active issue")]
    async fn set_issue_state(
        &self,
        Parameters(request): Parameters<SetStateRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .set_state_write(request, SubjectKind::Issue)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Replace the active issue labels")]
    async fn set_issue_labels(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .set_strings_write(request, SubjectKind::Issue, StringUpdate::Labels)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Replace the active issue assignees")]
    async fn set_issue_assignees(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .set_strings_write(request, SubjectKind::Issue, StringUpdate::Assignees)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(
        description = "Start implementation with one draft patch (draft: true) and link its ID \
                       briefly from the originating issue. Before changes, base and head may both \
                       be the current commit. Reuses the task's existing patch; continue it with \
                       update_patch. Describe decisions and validation in the patch, not again in \
                       the issue. Open the patch when ready. Request member review only for a \
                       specific concern needing that member's stronger expertise or when a \
                       delegate requests it; delegates otherwise review or assign reviewers."
    )]
    pub(super) async fn create_patch(
        &self,
        Parameters(request): Parameters<CreatePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.active().await?;
        self.require_implementation(&active).await?;
        self.authorize("create_patch").await?;
        let _creation = self.creations.lock().await;
        if let Some(patch) = self.existing_patch(&active).await? {
            self.scope.reconcile(active.context().activity).await?;
            self.record_relation(
                &active,
                crate::watcher::Subject::Patch(patch),
                ArtifactRole::Produced,
            )
            .await?;
            return Ok(Document::reused(
                patch.to_string(),
                "This task already has a patch; use update_patch for a revision or edit_patch for \
                 metadata",
            ));
        }
        let created = self
            .for_work(&active)
            .create_patch_write(request, active.context().workspace.clone())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(patch) = crate::mcp::protocol::written_id(&created) {
            self.record_relation(
                &active,
                crate::watcher::Subject::Patch(patch),
                ArtifactRole::Produced,
            )
            .await?;
        }
        Ok(created)
    }

    #[tool(
        description = "Add a revision to the existing patch from commits in the active workspace. \
                       Keep it in draft while implementing; set_patch_state to open when ready. \
                       Members with their own rejected review receive a follow-up then."
    )]
    pub(super) async fn update_patch(
        &self,
        Parameters(request): Parameters<UpdatePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.require_implementation(&active).await?;
        self.implementation_subject(&active, &request.subject)
            .await?;
        self.for_work(&active)
            .update_patch_write(request, active.context().workspace.clone())
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Edit active patch metadata")]
    async fn edit_patch(
        &self,
        Parameters(request): Parameters<EditPatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .edit_patch_write(request)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Set the active patch lifecycle")]
    pub(super) async fn set_patch_state(
        &self,
        Parameters(request): Parameters<SetStateRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        if request.state == "open" || request.state == "draft" {
            self.require_implementation(&active).await?;
            self.implementation_subject(&active, &request.subject)
                .await?;
        }
        self.for_work(&active)
            .set_state_write(request, SubjectKind::Patch)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Replace the active patch labels")]
    async fn set_patch_labels(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .set_strings_write(request, SubjectKind::Patch, StringUpdate::Labels)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Replace the active patch assignees")]
    async fn set_patch_assignees(
        &self,
        Parameters(request): Parameters<SetStringsRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .set_strings_write(request, SubjectKind::Patch, StringUpdate::Assignees)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(
        description = "Create a review for an active patch revision. Reject when changes are \
                       required before readiness; accept may include clearly optional nits. Keep \
                       the summary brief. Send each finding about specific code as a separate \
                       comment with its diff location; do not repeat those findings in the \
                       summary."
    )]
    pub(super) async fn create_review(
        &self,
        Parameters(request): Parameters<ReviewWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.verify_review(&active, &request).await?;
        self.for_work(&active)
            .review_write(request, false)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Edit an existing review on the active patch")]
    async fn edit_review(
        &self,
        Parameters(request): Parameters<ReviewWriteRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .review_write(request, true)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Record a merge attestation for the active patch")]
    async fn merge_patch(
        &self,
        Parameters(request): Parameters<MergePatchRequest>,
    ) -> Result<Json<Document>, String> {
        let active = self.access_subject(&request.subject).await?;
        self.for_work(&active)
            .merge_patch_write(request)
            .await
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Reply in the authenticated chat channel of the active message")]
    async fn reply_chat(
        &self,
        Parameters(request): Parameters<ReplyChatRequest>,
    ) -> Result<Json<ChatReplyRecord>, String> {
        self.authorize("reply_chat").await?;
        self.enqueue_chat_reply(request.text).await.map(Json)
    }

    pub(super) async fn enqueue_chat_reply(&self, text: String) -> Result<ChatReplyRecord, String> {
        let active = self.active().await?;
        let context = active.context();
        let (channel, message) = match (&context.source, context.activity) {
            (Source::Chat(channel), ActivityId::Chat(message)) => (channel, message),
            _ => return Err("reply_chat is available only for the active chat message".to_owned()),
        };
        let reply = self
            .chat
            .enqueue_reply(context.repository, channel, message, text)
            .await
            .map_err(|error| error.to_string())?;
        Ok(ChatReplyRecord { reply: reply.get() })
    }

    #[tool(
        description = "Record work for the building role of this member, which carries it out \
                       under the same identity. For review follow-up, identify each review by its \
                       ID and verified author, keep different authors' feedback separate, and pin \
                       the source entries in the task context."
    )]
    pub(super) async fn add_task(
        &self,
        Parameters(request): Parameters<AddTaskRequest>,
    ) -> Result<Json<TaskRecord>, String> {
        let active = self.active().await?;
        let submission = self.task_submission(&active, request).await?;
        let position = submission.position;
        let receipt = if let Some(activity) = active.context().delivery.clone() {
            self.queue.push_assignment(submission, activity).await
        } else {
            self.queue.push(submission).await
        }
        .map_err(|error| error.to_string())?;
        let position = if matches!(
            receipt.state,
            crate::queue::TaskState::Waiting | crate::queue::TaskState::Running
        ) {
            match self.queue.get(receipt.task).await {
                Ok(task) => task.position,
                Err(crate::queue::Error::MissingTask(_)) => position,
                Err(error) => return Err(error.to_string()),
            }
        } else {
            position
        };
        let waiting = self
            .queue
            .waiting()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(TaskRecord {
            task: receipt.task,
            position,
            waiting,
            state: receipt.state,
            result: receipt.result,
        }))
    }

    #[tool(
        description = "Cancel the specified waiting task or ask that running task to preserve its \
                       work and stop"
    )]
    pub(super) async fn cancel(
        &self,
        Parameters(request): Parameters<TaskRequest>,
    ) -> Result<Json<TaskAction>, String> {
        self.task_action(request.task, self.queue.cancel(request.task))
            .await
    }

    #[tool(
        description = "Finalize an explicit implementation handoff after the other member's \
                       signed receipt confirms it is queued. A review assignment is not \
                       acceptance of implementation. Cancels only your related waiting task, or \
                       narrows its specification for a partial transfer. Running work must be \
                       stopped and preserved separately."
    )]
    pub(super) async fn handoff_task(
        &self,
        Parameters(request): Parameters<HandoffRequest>,
    ) -> Result<Json<TaskAction>, String> {
        self.handoff(request).await
    }

    #[tool(description = "Save a correction for the specified running or waiting task")]
    pub(super) async fn note(
        &self,
        Parameters(request): Parameters<NoteRequest>,
    ) -> Result<Json<TaskAction>, String> {
        self.task_action(request.task, self.queue.note(request.task, request.note))
            .await
    }

    #[tool(
        description = "Read a task's durable specification, related artifacts, pinned messages, \
                       and notes"
    )]
    pub(super) async fn get_task_context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<Json<TaskSummary>, String> {
        let active = self.active().await?;
        let id = active.task_id(request.task)?;
        self.queue
            .get(id)
            .await
            .map(TaskSummary::from)
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        description = "Save specification corrections, artifact references, pinned messages, and \
                       notes for a task; references do not grant write permission"
    )]
    pub(super) async fn update_task_context(
        &self,
        Parameters(request): Parameters<ContextUpdateRequest>,
    ) -> Result<Json<TaskSummary>, String> {
        let active = self.active().await?;
        let id = active.task_id(request.task)?;
        self.task_access(&active, id).await?;
        self.validate_context(&request.update).await?;
        self.queue
            .update_context(id, request.update)
            .await
            .map(TaskSummary::from)
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(description = "Report what this member is building and what is waiting")]
    async fn task_status(&self) -> Result<Json<TaskStatus>, String> {
        let _active = self.active().await?;
        let waiting = self
            .queue
            .pending()
            .await
            .map_err(|error| error.to_string())?;

        Ok(Json(TaskStatus {
            running: self.queue.running().await.map(TaskSummary::from),
            waiting: waiting.into_iter().map(TaskSummary::from).collect(),
        }))
    }

    pub(super) async fn pending_notes(&self) -> Vec<String> {
        match self.role {
            Role::Coder => match self.scope.access_current().await {
                Ok(active) => match active.context().activity {
                    ActivityId::Task(task) => self.queue.notes(task).await,
                    _ => Vec::new(),
                },
                Err(_) => Vec::new(),
            },
            Role::Talker | Role::Chatter => Vec::new(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ToolService {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call = ToolCallContext::new(self, request, context);
        let mut result = self.tool_router.call(call).await?;
        for note in self.pending_notes().await {
            result.content.push(ContentBlock::text(format!(
                "Note from the conversation: {note}"
            )));
        }

        Ok(result)
    }
}
