//! Radicle mutations backing MCP tools.

use std::str::FromStr as _;

use radicle::{
    cob::{
        EntryId, ObjectId,
        issue::{self, Issues},
        patch::{self, Patches},
        store::access::ReadOnly,
    },
    storage::{ReadRepository as _, ReadStorage as _},
};
use rmcp::Json;

use crate::{
    agent::PublishError,
    mcp::{
        protocol::{
            CommentIssueRequest, CommentOperation, CommentTargetKind, CommentWriteRequest,
            CreateIssueRequest, CreatePatchRequest, Document, EditIssueRequest, EditPatchRequest,
            IssueUpdate, MergePatchRequest, PatchCommitAction, PatchCommits, PatchUpdate,
            PreparedEmbed, ReactionWriteRequest, ResolveCommentRequest, ReviewWriteRequest,
            SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind, SubjectRequest,
            SubjectUpdate, UpdatePatchRequest, import_commits, mutation_identity, parse_dids,
            parse_entry_string, parse_issue_state, parse_labels, parse_location,
            parse_object_string, parse_patch_commits, parse_patch_state, parse_review,
            parse_revision, parse_subject_request, parse_verdict, require_kind,
            validate_code_location, validate_merge, write_result,
        },
        recovery::{
            CommentRecovery, ReactionRecovery, find_review, issue_create_operation,
            issue_edit_operation, issue_update_operation, merge_operation, patch_commit_operation,
            patch_edit_operation, patch_update_operation, review_mutation_operation,
            review_resolution_operation,
        },
        server::ServiceError,
        tools::ToolService,
    },
};

impl ToolService {
    async fn require_issue_delegate(&self, operation: &'static str) -> Result<(), ServiceError> {
        let delegate = self
            .blocking(|service| {
                let repository = service.storage.repository(service.repository)?;
                Ok(repository
                    .identity_doc()?
                    .is_delegate(&service.publisher.author().into()))
            })
            .await?;
        if delegate {
            Ok(())
        } else {
            Err(ServiceError::IssueDelegate(operation))
        }
    }

    async fn mutate<T, F, V>(
        &self,
        method: &'static str,
        request: String,
        operation: F,
        validate: V,
    ) -> Result<String, ServiceError>
    where
        T: Send + ToString + 'static,
        F: FnOnce(
                &radicle::storage::git::Repository,
                &radicle::node::device::BoxedDevice,
                &str,
            ) -> Result<T, PublishError>
            + Send
            + 'static,
        V: Fn(
                &radicle::storage::git::Repository,
                radicle::crypto::PublicKey,
                EntryId,
                crate::agent::MutationValidation,
            ) -> Result<bool, PublishError>
            + Send
            + 'static,
    {
        let mutation = if let Some(activity) = self.mutation_activity {
            let invocations = self
                .mutation_invocations
                .as_ref()
                .expect("activity invocation counters should be configured");
            let mut invocations = invocations.lock().await;
            let invocation = invocations.entry(request.clone()).or_default();
            let key = crate::agent::MutationKey {
                repository: self
                    .mutation_repository
                    .expect("activity repository should be configured"),
                activity,
                invocation: *invocation,
                request,
            };
            *invocation += 1;
            Some(key)
        } else {
            None
        };
        self.publisher
            .mutate(method, mutation, operation, validate)
            .await
            .map_err(ServiceError::from)
    }

    pub(super) async fn write_comment(
        &self,
        request: CommentWriteRequest,
        operation: CommentOperation,
    ) -> Result<Json<Document>, ServiceError> {
        let method = match operation {
            CommentOperation::Create => "create_comment",
            CommentOperation::Edit => "edit_comment",
            CommentOperation::Redact => "redact_comment",
        };
        let request_id = mutation_identity(method, &request)?;
        self.write_comment_as(request, operation, method, request_id)
            .await
    }

    pub(super) async fn comment_issue_write(
        &self,
        request: CommentIssueRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("comment_issue", &request)?;
        self.write_comment_as(
            request.comment(),
            CommentOperation::Create,
            "comment_issue",
            request_id,
        )
        .await
    }

    async fn write_comment_as(
        &self,
        request: CommentWriteRequest,
        operation: CommentOperation,
        method: &'static str,
        request_id: String,
    ) -> Result<Json<Document>, ServiceError> {
        let subject = parse_object_string(&request.subject.subject)?;
        let entity = request
            .entity
            .as_deref()
            .map(parse_entry_string)
            .transpose()?;
        let comment = request
            .comment
            .as_deref()
            .map(parse_entry_string)
            .transpose()?;
        let reply_to = request
            .reply_to
            .as_deref()
            .map(parse_entry_string)
            .transpose()?;
        let body = request.body.clone();
        let location = request.location.map(parse_location).transpose()?;
        let embeds = PreparedEmbed::prepare(request.embeds)?;
        validate_comment_request(
            operation,
            request.target,
            body.as_deref(),
            comment,
            embeds.is_empty(),
            reply_to.is_some(),
        )?;
        let kind = request.subject.kind;
        let target = request.target;
        let expected_body = body.clone();
        let expected_embeds = PreparedEmbed::references(&embeds);
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| match (kind, target) {
                    (SubjectKind::Issue, CommentTargetKind::Issue) => {
                        let mut issues = issue::Cache::no_cache(repository, signer)?;
                        let mut issue = issues.get_mut(&subject)?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && issue.thread().comment(&comment).is_none()
                        {
                            return Err(PublishError::Rejected("comment was not found".to_owned()));
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && issue.thread().comment(&parent).is_none()
                        {
                            return Err(PublishError::Rejected(
                                "reply parent was not found".to_owned(),
                            ));
                        }
                        let embeds = PreparedEmbed::store(&embeds, repository)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        issue
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.comment(
                                    body.expect("body was validated"),
                                    reply_to.unwrap_or(*subject),
                                    embeds,
                                ),
                                CommentOperation::Edit => tx.edit_comment(
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    embeds,
                                ),
                                CommentOperation::Redact => {
                                    tx.redact_comment(comment.expect("comment was validated"))
                                }
                            })
                            .map_err(PublishError::from)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Revision) => {
                        let revision = parse_revision(entity.ok_or_else(|| {
                            PublishError::Rejected("entity revision is required".to_owned())
                        })?)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&subject)?;
                        let revision_value = patch
                            .revision(&revision)
                            .ok_or_else(|| {
                                ServiceError::Request("revision was not found".to_owned())
                            })
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && revision_value.discussion().comment(&comment).is_none()
                        {
                            return Err(PublishError::Rejected("comment was not found".to_owned()));
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && revision_value.discussion().comment(&parent).is_none()
                        {
                            return Err(PublishError::Rejected(
                                "reply parent was not found".to_owned(),
                            ));
                        }
                        if let Some(location) = location.as_ref() {
                            validate_code_location(repository, revision_value, location)
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        }
                        let embeds = PreparedEmbed::store(&embeds, repository)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        patch
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.comment(
                                    revision,
                                    body.expect("body was validated"),
                                    reply_to,
                                    location,
                                    embeds,
                                ),
                                CommentOperation::Edit => tx.comment_edit(
                                    revision,
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    embeds,
                                ),
                                CommentOperation::Redact => tx.comment_redact(
                                    revision,
                                    comment.expect("comment was validated"),
                                ),
                            })
                            .map_err(PublishError::from)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Review) => {
                        let review = parse_review(entity.ok_or_else(|| {
                            PublishError::Rejected("entity review is required".to_owned())
                        })?)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&subject)?;
                        let review_value = find_review(&patch, review)
                            .ok_or_else(|| ServiceError::Request("review was not found".to_owned()))
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && !review_value
                                .comments()
                                .any(|(candidate, _value)| *candidate == comment)
                        {
                            return Err(PublishError::Rejected("comment was not found".to_owned()));
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && !review_value
                                .comments()
                                .any(|(candidate, _value)| *candidate == parent)
                        {
                            return Err(PublishError::Rejected(
                                "reply parent was not found".to_owned(),
                            ));
                        }
                        if let Some(location) = location.as_ref() {
                            let revision = patch
                                .revisions()
                                .find_map(|(revision, value)| {
                                    patch
                                        .reviews_of(revision)
                                        .any(|(candidate, _)| *candidate == review)
                                        .then_some(value)
                                })
                                .ok_or_else(|| {
                                    ServiceError::Request("review was not found".to_owned())
                                })
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                            validate_code_location(repository, revision, location)
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        }
                        let embeds = PreparedEmbed::store(&embeds, repository)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        patch
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.review_comment(
                                    review,
                                    body.expect("body was validated"),
                                    location,
                                    reply_to,
                                    embeds,
                                ),
                                CommentOperation::Edit => tx.edit_review_comment(
                                    review,
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    embeds,
                                ),
                                CommentOperation::Redact => tx.redact_review_comment(
                                    review,
                                    comment.expect("comment was validated"),
                                ),
                            })
                            .map_err(PublishError::from)
                    }
                    _ => Err(PublishError::Rejected(
                        "comment target does not match subject".to_owned(),
                    )),
                },
                move |repository, _author, recovered, validation| {
                    let recovery = CommentRecovery {
                        kind,
                        target,
                        subject,
                        entity,
                        comment: match operation {
                            CommentOperation::Create => recovered,
                            CommentOperation::Edit | CommentOperation::Redact => {
                                comment.expect("comment was validated")
                            }
                        },
                        operation,
                        body: expected_body.as_deref(),
                        embeds: &expected_embeds,
                    };
                    match validation {
                        crate::agent::MutationValidation::Fresh => recovery.applied(repository),
                        crate::agent::MutationValidation::Recovery => {
                            recovery.operation_recorded(repository, recovered)
                        }
                    }
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn resolve_review_comment(
        &self,
        request: ResolveCommentRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("resolve_comment", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let review = patch::ReviewId::from(parse_entry_string(&request.review)?);
        let comment = parse_entry_string(&request.comment)?;
        let resolved = request.resolved;
        let id = self
            .mutate(
                "resolve_comment",
                request_id,
                move |repository, signer, marker| {
                    let mut patches = patch::Cache::no_cache(repository, signer)?;
                    let mut patch = patches.get_mut(&patch_id)?;
                    patch
                        .transaction(marker, |tx| {
                            if resolved {
                                tx.review_comment_resolve(review, comment)
                            } else {
                                tx.review_comment_unresolve(review, comment)
                            }
                        })
                        .map_err(PublishError::from)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return review_resolution_operation(
                            repository, patch_id, operation, review, comment, resolved,
                        );
                    }
                    let patch = Patches::open(repository, ReadOnly)?.get(&patch_id)?;
                    Ok(patch.is_some_and(|patch| {
                        find_review(&patch, review)
                            .and_then(|review| {
                                review.comments().find_map(|(candidate, value)| {
                                    (*candidate == comment).then_some(value)
                                })
                            })
                            .is_some_and(|comment| comment.is_resolved() == resolved)
                    }))
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn reaction_write(
        &self,
        request: ReactionWriteRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("set_reaction", &request)?;
        let subject = parse_object_string(&request.subject.subject)?;
        let entity = request
            .entity
            .as_deref()
            .map(parse_entry_string)
            .transpose()?;
        let comment = request
            .comment
            .as_deref()
            .map(parse_entry_string)
            .transpose()?;
        let reaction = request.parsed_reaction()?;
        let location = request.location.map(parse_location).transpose()?;
        let kind = request.subject.kind;
        let target = request.target;
        let active = request.active;
        let expected_location = location.clone();
        let id = self
            .mutate(
                "set_reaction",
                request_id,
                move |repository, signer, marker| match (kind, target) {
                    (SubjectKind::Issue, CommentTargetKind::Issue) => {
                        if entity.is_some() || location.is_some() {
                            return Err(PublishError::Rejected(
                                "issue reactions do not accept entity or location".to_owned(),
                            ));
                        }
                        let comment = comment.unwrap_or(*subject);
                        let mut issues = issue::Cache::no_cache(repository, signer)?;
                        let mut issue = issues.get_mut(&subject)?;
                        if issue.thread().comment(&comment).is_none() {
                            return Err(PublishError::Rejected(
                                "reaction target was not found".to_owned(),
                            ));
                        }
                        issue
                            .transaction(marker, |tx| tx.react(comment, reaction, active))
                            .map_err(PublishError::from)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Revision) => {
                        let revision = parse_revision(entity.ok_or_else(|| {
                            PublishError::Rejected("entity revision is required".to_owned())
                        })?)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if comment.is_some() && location.is_some() {
                            return Err(PublishError::Rejected(
                                "revision comment reactions do not accept location".to_owned(),
                            ));
                        }
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&subject)?;
                        let revision_value = patch
                            .revision(&revision)
                            .ok_or_else(|| {
                                ServiceError::Request("revision was not found".to_owned())
                            })
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if let Some(comment) = comment
                            && revision_value.discussion().comment(&comment).is_none()
                        {
                            return Err(PublishError::Rejected(
                                "reaction target was not found".to_owned(),
                            ));
                        }
                        if let Some(location) = location.as_ref() {
                            validate_code_location(repository, revision_value, location)
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        }
                        patch
                            .transaction(marker, |tx| match comment {
                                Some(comment) => {
                                    tx.comment_react(revision, comment, reaction, active)
                                }
                                None => tx.react(revision, reaction, location, active),
                            })
                            .map_err(PublishError::from)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Review) => {
                        if location.is_some() {
                            return Err(PublishError::Rejected(
                                "review reactions do not accept location".to_owned(),
                            ));
                        }
                        let review = parse_review(entity.ok_or_else(|| {
                            PublishError::Rejected("entity review is required".to_owned())
                        })?)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&subject)?;
                        let review_value = find_review(&patch, review)
                            .ok_or_else(|| ServiceError::Request("review was not found".to_owned()))
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if let Some(comment) = comment
                            && !review_value
                                .comments()
                                .any(|(candidate, _value)| *candidate == comment)
                        {
                            return Err(PublishError::Rejected(
                                "reaction target was not found".to_owned(),
                            ));
                        }
                        patch
                            .transaction(marker, |tx| match comment {
                                Some(comment) => {
                                    tx.react_review_comment(review, comment, reaction, active)
                                }
                                None => tx.review_react(review, reaction, active),
                            })
                            .map_err(PublishError::from)
                    }
                    _ => Err(PublishError::Rejected(
                        "reaction target does not match subject".to_owned(),
                    )),
                },
                move |repository, author, operation, validation| {
                    let recovery = ReactionRecovery {
                        kind,
                        target,
                        subject,
                        entity,
                        comment,
                        reaction,
                        location: expected_location.as_ref(),
                        active,
                    };
                    match validation {
                        crate::agent::MutationValidation::Fresh => {
                            recovery.applied(repository, author)
                        }
                        crate::agent::MutationValidation::Recovery => {
                            recovery.operation_recorded(repository, operation)
                        }
                    }
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn create_issue_write(
        &self,
        request: CreateIssueRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("create_issue", &request)?;
        self.create_issue_as(request, "create_issue", request_id, true)
            .await
    }

    pub(super) async fn create_security_issue_write(
        &self,
        request: CreateIssueRequest,
        request_id: String,
    ) -> Result<Json<Document>, ServiceError> {
        self.create_issue_as(request, "report_security_issue", request_id, false)
            .await
    }

    async fn create_issue_as(
        &self,
        request: CreateIssueRequest,
        method: &'static str,
        request_id: String,
        recover_description: bool,
    ) -> Result<Json<Document>, ServiceError> {
        let title = radicle::cob::Title::try_from(request.title)?;
        let labels = parse_labels(request.labels)?;
        let assignees = parse_dids(request.assignees)?;
        let description = request.description;
        if !assignees.is_empty() || !labels.is_empty() {
            self.require_issue_delegate("assignees or labels").await?;
        }
        let expected_title = title.clone();
        let expected_labels = labels.clone();
        let expected_assignees = assignees.clone();
        let expected_description = description.clone();
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| {
                    let identity = repository.identity_head()?;
                    let mut issues = radicle::cob::store::Store::<issue::Issue, _, _>::open(
                        repository,
                        radicle::cob::store::access::WriteAs::new(signer),
                    )?
                    .identity(identity);
                    let (id, _issue) = radicle::cob::store::Transaction::initial(
                        marker,
                        &mut issues,
                        |tx: &mut radicle::cob::store::Transaction<issue::Issue, _>,
                         _repository| {
                            tx.push(issue::Action::Comment {
                                body: description,
                                reply_to: None,
                                embeds: Vec::new(),
                            })?;
                            tx.push(issue::Action::Edit { title })?;
                            if !assignees.is_empty() {
                                tx.push(issue::Action::Assign {
                                    assignees: assignees.into_iter().collect(),
                                })?;
                            }
                            if !labels.is_empty() {
                                tx.push(issue::Action::Label {
                                    labels: labels.into_iter().collect(),
                                })?;
                            }
                            Ok(())
                        },
                    )?;
                    Ok(id)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return issue_create_operation(
                            repository,
                            operation,
                            &expected_title,
                            recover_description.then_some(expected_description.as_str()),
                            &expected_labels,
                            &expected_assignees,
                        );
                    }
                    Issues::open(repository, ReadOnly)?
                        .get(&ObjectId::from(operation))
                        .map(|issue| issue.is_some())
                        .map_err(PublishError::from)
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn edit_issue_write(
        &self,
        request: EditIssueRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("edit_issue", &request)?;
        require_kind(request.subject.kind, SubjectKind::Issue)?;
        let issue_id = parse_object_string(&request.subject.subject)?;
        let title = request
            .title
            .map(radicle::cob::Title::try_from)
            .transpose()?;
        let description = request.description;
        let expected_title = title.clone();
        let expected_description = description.clone();
        if title.is_none() && description.is_none() {
            return Err(ServiceError::Request(
                "title or description is required".to_owned(),
            ));
        }
        let id = self
            .mutate(
                "edit_issue",
                request_id,
                move |repository, signer, marker| {
                    let mut issues = issue::Cache::no_cache(repository, signer)?;
                    let mut issue = issues.get_mut(&issue_id)?;
                    issue
                        .transaction(marker, |tx| {
                            if let Some(title) = title {
                                tx.edit(title)?;
                            }
                            if let Some(description) = description {
                                tx.edit_comment(*issue_id, description, Vec::new())?;
                            }
                            Ok(())
                        })
                        .map_err(PublishError::from)
                },
                move |repository, _author, operation, _validation| {
                    issue_edit_operation(
                        repository,
                        issue_id,
                        operation,
                        expected_title.as_ref(),
                        expected_description.as_deref(),
                    )
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn create_patch_write(
        &self,
        request: CreatePatchRequest,
        workspace: std::path::PathBuf,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("create_patch", &request)?;
        let title = radicle::cob::Title::try_from(request.title)?;
        let labels = parse_labels(request.labels)?;
        let commits =
            parse_patch_commits(request.description, request.base, request.head, workspace)?;
        self.patch_commit_write(
            "create_patch",
            request_id,
            commits,
            PatchCommitAction::Create {
                title,
                labels,
                draft: request.draft,
            },
        )
        .await
    }

    pub(super) async fn update_patch_write(
        &self,
        request: UpdatePatchRequest,
        workspace: std::path::PathBuf,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("update_patch", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let commits =
            parse_patch_commits(request.description, request.base, request.head, workspace)?;
        self.patch_commit_write(
            "update_patch",
            request_id,
            commits,
            PatchCommitAction::Update(patch_id),
        )
        .await
    }

    async fn patch_commit_write(
        &self,
        method: &'static str,
        request_id: String,
        commits: PatchCommits,
        action: PatchCommitAction,
    ) -> Result<Json<Document>, ServiceError> {
        let namespace = self.publisher.author();
        let expected_action = action.clone();
        let expected_description = commits.description.clone();
        let expected_base = commits.base;
        let expected_head = commits.head;
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| {
                    import_commits(
                        repository,
                        &commits.workspace,
                        namespace,
                        &[commits.base, commits.head],
                    )
                    .map_err(|error| PublishError::Rejected(error.to_string()))?;
                    match action {
                        PatchCommitAction::Create {
                            title,
                            labels,
                            draft,
                        } => {
                            let identity = repository.identity_head()?;
                            let mut patches =
                                radicle::cob::store::Store::<patch::Patch, _, _>::open(
                                    repository,
                                    radicle::cob::store::access::WriteAs::new(signer),
                                )?
                                .identity(identity);
                            let (id, _patch) = radicle::cob::store::Transaction::initial(
                                marker,
                                &mut patches,
                                |tx: &mut radicle::cob::store::Transaction<patch::Patch, _>,
                                 _repository| {
                                    tx.revision(commits.description, commits.base, commits.head)?;
                                    tx.push(patch::Action::Edit {
                                        title,
                                        target: patch::MergeTarget::Delegates,
                                    })?;
                                    if !labels.is_empty() {
                                        tx.label(labels)?;
                                    }
                                    if draft {
                                        tx.lifecycle(patch::Lifecycle::Draft)?;
                                    }
                                    Ok(())
                                },
                            )?;
                            Ok(id.to_string())
                        }
                        PatchCommitAction::Update(id) => {
                            let mut patches = patch::Cache::no_cache(repository, signer)?;
                            let mut patch = patches.get_mut(&id)?;
                            patch
                                .transaction(marker, |tx| {
                                    tx.revision(commits.description, commits.base, commits.head)
                                })
                                .map(|id| id.to_string())
                                .map_err(PublishError::from)
                        }
                    }
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return patch_commit_operation(
                            repository,
                            &expected_action,
                            operation,
                            &expected_description,
                            expected_base,
                            expected_head,
                        );
                    }
                    let patches = Patches::open(repository, ReadOnly)?;
                    match expected_action {
                        PatchCommitAction::Create { .. } => patches
                            .get(&ObjectId::from(operation))
                            .map(|patch| patch.is_some())
                            .map_err(PublishError::from),
                        PatchCommitAction::Update(id) => {
                            let patch = patches.get(&id)?;
                            Ok(patch.is_some_and(|patch| {
                                patch
                                    .revision(&patch::RevisionId::from(operation))
                                    .is_some_and(|revision| {
                                        revision.description() == expected_description
                                            && *revision.base() == expected_base
                                            && revision.head() == expected_head
                                    })
                            }))
                        }
                    }
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn edit_patch_write(
        &self,
        request: EditPatchRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("edit_patch", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let title = request
            .title
            .map(radicle::cob::Title::try_from)
            .transpose()?;
        let revision = request
            .revision
            .as_deref()
            .map(parse_entry_string)
            .transpose()?
            .map(patch::RevisionId::from);
        let description = request.description;
        let expected_title = title.clone();
        let expected_revision = revision;
        let expected_description = description.clone();
        if title.is_none() && description.is_none() {
            return Err(ServiceError::Request(
                "title or description is required".to_owned(),
            ));
        }
        if description.is_some() && revision.is_none() {
            return Err(ServiceError::Request(
                "revision is required when editing a description".to_owned(),
            ));
        }
        let id = self
            .mutate(
                "edit_patch",
                request_id,
                move |repository, signer, marker| {
                    let mut patches = patch::Cache::no_cache(repository, signer)?;
                    let mut patch = patches.get_mut(&patch_id)?;
                    patch
                        .transaction(marker, |tx| {
                            if let Some(title) = title {
                                tx.edit(title, patch::MergeTarget::Delegates)?;
                            }
                            if let (Some(description), Some(revision)) = (description, revision) {
                                tx.edit_revision(revision, description, Vec::new())?;
                            }
                            Ok(())
                        })
                        .map_err(PublishError::from)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return patch_edit_operation(
                            repository,
                            patch_id,
                            operation,
                            expected_title.as_ref(),
                            expected_revision,
                            expected_description.as_deref(),
                        );
                    }
                    let patch = Patches::open(repository, ReadOnly)?.get(&patch_id)?;
                    Ok(patch.is_some_and(|patch| {
                        expected_title
                            .as_ref()
                            .is_none_or(|title| patch.title() == title.as_ref())
                            && expected_description.as_deref().is_none_or(|description| {
                                expected_revision
                                    .and_then(|revision| patch.revision(&revision))
                                    .is_some_and(|revision| revision.description() == description)
                            })
                    }))
                },
            )
            .await?;
        write_result(id)
    }

    async fn subject_update(
        &self,
        method: &'static str,
        request_id: String,
        subject: SubjectRequest,
        update: SubjectUpdate,
    ) -> Result<Json<Document>, ServiceError> {
        let expected_subject = subject.clone();
        let expected_update = update.clone();
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| match update {
                    SubjectUpdate::Issue(update) => {
                        let id = parse_subject_request(&subject, SubjectKind::Issue)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let mut issues = issue::Cache::no_cache(repository, signer)?;
                        let mut issue = issues.get_mut(&id)?;
                        issue
                            .transaction(marker, |tx| match update {
                                IssueUpdate::State(state) => tx.lifecycle(state),
                                IssueUpdate::Labels(labels) => tx.label(labels),
                                IssueUpdate::Assignees(assignees) => tx.assign(assignees),
                            })
                            .map_err(PublishError::from)
                    }
                    SubjectUpdate::Patch(update) => {
                        let id = parse_subject_request(&subject, SubjectKind::Patch)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&id)?;
                        patch
                            .transaction(marker, |tx| match update {
                                PatchUpdate::State(state) => tx.lifecycle(state),
                                PatchUpdate::Labels(labels) => tx.label(labels),
                                PatchUpdate::Assignees(assignees) => tx.assign(assignees),
                            })
                            .map_err(PublishError::from)
                    }
                },
                move |repository, _author, operation, validation| match &expected_update {
                    SubjectUpdate::Issue(update) => {
                        if matches!(validation, crate::agent::MutationValidation::Recovery) {
                            let id = parse_subject_request(&expected_subject, SubjectKind::Issue)
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                            return issue_update_operation(repository, id, operation, update);
                        }
                        let id = parse_subject_request(&expected_subject, SubjectKind::Issue)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let issue = Issues::open(repository, ReadOnly)?.get(&id)?;
                        Ok(issue.is_some_and(|issue| match update {
                            IssueUpdate::State(state) => issue.state() == state,
                            IssueUpdate::Labels(labels) => {
                                issue.labels().count() == labels.len()
                                    && labels
                                        .iter()
                                        .all(|label| issue.labels().any(|v| v == label))
                            }
                            IssueUpdate::Assignees(assignees) => {
                                issue.assignees().count() == assignees.len()
                                    && assignees
                                        .iter()
                                        .all(|did| issue.assignees().any(|v| v == did))
                            }
                        }))
                    }
                    SubjectUpdate::Patch(update) => {
                        if matches!(validation, crate::agent::MutationValidation::Recovery) {
                            let id = parse_subject_request(&expected_subject, SubjectKind::Patch)
                                .map_err(|error| PublishError::Rejected(error.to_string()))?;
                            return patch_update_operation(repository, id, operation, update);
                        }
                        let id = parse_subject_request(&expected_subject, SubjectKind::Patch)
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        let patch = Patches::open(repository, ReadOnly)?.get(&id)?;
                        Ok(patch.is_some_and(|patch| match update {
                            PatchUpdate::State(state) => match state {
                                patch::Lifecycle::Open => {
                                    matches!(patch.state(), patch::State::Open { .. })
                                }
                                patch::Lifecycle::Draft => {
                                    matches!(patch.state(), patch::State::Draft)
                                }
                                patch::Lifecycle::Archived => {
                                    matches!(patch.state(), patch::State::Archived)
                                }
                            },
                            PatchUpdate::Labels(labels) => {
                                patch.labels().count() == labels.len()
                                    && labels
                                        .iter()
                                        .all(|label| patch.labels().any(|v| v == label))
                            }
                            PatchUpdate::Assignees(assignees) => {
                                patch.assignees().count() == assignees.len()
                                    && assignees
                                        .iter()
                                        .all(|did| patch.assignees().any(|v| &v == did))
                            }
                        }))
                    }
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn set_state_write(
        &self,
        request: SetStateRequest,
        kind: SubjectKind,
    ) -> Result<Json<Document>, ServiceError> {
        let (method, update) = match kind {
            SubjectKind::Issue => (
                "set_issue_state",
                SubjectUpdate::Issue(IssueUpdate::State(parse_issue_state(&request.state)?)),
            ),
            SubjectKind::Patch => (
                "set_patch_state",
                SubjectUpdate::Patch(PatchUpdate::State(parse_patch_state(&request.state)?)),
            ),
        };
        let request_id = mutation_identity(method, &request)?;
        self.subject_update(method, request_id, request.subject, update)
            .await
    }

    pub(super) async fn set_strings_write(
        &self,
        request: SetStringsRequest,
        kind: SubjectKind,
        change: StringUpdate,
    ) -> Result<Json<Document>, ServiceError> {
        let method = match (kind, change) {
            (SubjectKind::Issue, StringUpdate::Labels) => "set_issue_labels",
            (SubjectKind::Issue, StringUpdate::Assignees) => "set_issue_assignees",
            (SubjectKind::Patch, StringUpdate::Labels) => "set_patch_labels",
            (SubjectKind::Patch, StringUpdate::Assignees) => "set_patch_assignees",
        };
        if kind == SubjectKind::Issue {
            self.require_issue_delegate(match change {
                StringUpdate::Labels => "labels",
                StringUpdate::Assignees => "assignees",
            })
            .await?;
        }
        let request_id = mutation_identity(method, &request)?;
        let update = match (kind, change) {
            (SubjectKind::Issue, StringUpdate::Labels) => {
                SubjectUpdate::Issue(IssueUpdate::Labels(parse_labels(request.values)?))
            }
            (SubjectKind::Issue, StringUpdate::Assignees) => {
                SubjectUpdate::Issue(IssueUpdate::Assignees(parse_dids(request.values)?))
            }
            (SubjectKind::Patch, StringUpdate::Labels) => {
                SubjectUpdate::Patch(PatchUpdate::Labels(parse_labels(request.values)?))
            }
            (SubjectKind::Patch, StringUpdate::Assignees) => SubjectUpdate::Patch(
                PatchUpdate::Assignees(parse_dids(request.values)?.into_iter().collect()),
            ),
        };
        self.subject_update(method, request_id, request.subject, update)
            .await
    }

    pub(super) async fn review_write(
        &self,
        request: ReviewWriteRequest,
        edit: bool,
    ) -> Result<Json<Document>, ServiceError> {
        let method = if edit { "edit_review" } else { "create_review" };
        let request_id = mutation_identity(method, &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let revision = request
            .revision
            .as_deref()
            .map(parse_entry_string)
            .transpose()?
            .map(patch::RevisionId::from);
        let review = request
            .review
            .as_deref()
            .map(parse_entry_string)
            .transpose()?
            .map(patch::ReviewId::from);
        let verdict = parse_verdict(request.verdict.as_deref())?;
        let summary = request.summary;
        let labels = parse_labels(request.labels)?;
        let embeds = PreparedEmbed::prepare(request.embeds)?;
        let expected_review = review;
        let expected_verdict = verdict;
        let expected_summary = summary.clone();
        let expected_labels = labels.clone();
        let expected_embeds = PreparedEmbed::references(&embeds);
        if edit && review.is_none() {
            return Err(ServiceError::Request("review is required".to_owned()));
        }
        if edit && summary.is_none() {
            return Err(ServiceError::Request("summary is required".to_owned()));
        }
        if !edit && revision.is_none() {
            return Err(ServiceError::Request("revision is required".to_owned()));
        }
        if !edit && verdict.is_none() && summary.is_none() {
            return Err(ServiceError::Request(
                "verdict or summary is required".to_owned(),
            ));
        }
        if !edit && !embeds.is_empty() {
            return Err(ServiceError::Request(
                "create_review cannot include embeds; create it, then use edit_review".to_owned(),
            ));
        }
        if edit && !request.comments.is_empty() {
            return Err(ServiceError::Request(
                "edit_review cannot include line comments; publish them with create_review"
                    .to_owned(),
            ));
        }
        let comments = request
            .comments
            .into_iter()
            .map(|comment| {
                if comment.body.trim().is_empty() {
                    return Err(ServiceError::Request(
                        "a review comment body cannot be empty".to_owned(),
                    ));
                }
                let location = comment.location.map(parse_location).transpose()?;
                Ok((comment.body, location))
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| {
                    let mut patches = patch::Cache::no_cache(repository, signer)?;
                    let mut patch = patches.get_mut(&patch_id)?;
                    if edit {
                        let review = review.expect("review was validated");
                        let review = find_review(&patch, review)
                            .ok_or_else(|| ServiceError::Request("review was not found".to_owned()))
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if review.author().public_key() != signer.public_key() {
                            return Err(PublishError::Rejected(
                                "review belongs to another author".to_owned(),
                            ));
                        }
                    } else {
                        let revision = revision.expect("revision was validated");
                        let revision = patch
                            .revision(&revision)
                            .ok_or_else(|| {
                                ServiceError::Request("revision was not found".to_owned())
                            })
                            .map_err(|error| PublishError::Rejected(error.to_string()))?;
                        if revision
                            .reviews()
                            .any(|(author, _review)| author == signer.public_key())
                        {
                            return Err(PublishError::Rejected(
                                "revision was already reviewed by this author".to_owned(),
                            ));
                        }
                    }
                    let embeds = PreparedEmbed::store(&embeds, repository)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                    patch
                        .transaction(marker, |tx| {
                            if edit {
                                tx.embed(embeds.clone())?;
                                tx.review_edit(
                                    review.expect("review was validated"),
                                    verdict,
                                    summary.expect("summary was validated"),
                                    labels,
                                    embeds,
                                )
                            } else {
                                tx.review(
                                    revision.expect("revision was validated"),
                                    verdict,
                                    summary,
                                    labels,
                                )
                            }
                        })
                        .map_err(PublishError::from)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return review_mutation_operation(
                            repository,
                            patch_id,
                            operation,
                            edit,
                            revision,
                            expected_review,
                            expected_verdict,
                            expected_summary.as_deref(),
                            &expected_labels,
                            &expected_embeds,
                        );
                    }
                    let patch = Patches::open(repository, ReadOnly)?.get(&patch_id)?;
                    let id = if edit {
                        expected_review.expect("review was validated")
                    } else {
                        patch::ReviewId::from(operation)
                    };
                    Ok(patch
                        .as_ref()
                        .and_then(|patch| find_review(patch, id))
                        .is_some_and(|review| {
                            review.verdict() == expected_verdict
                                && review.summary()
                                    == expected_summary.as_deref().unwrap_or_default()
                                && review.embeds() == expected_embeds
                                && review.labels().count() == expected_labels.len()
                                && expected_labels
                                    .iter()
                                    .all(|label| review.labels().any(|value| value == label))
                        }))
                },
            )
            .await?;
        if !comments.is_empty() {
            self.write_review_comments(&id, patch_id, comments).await?;
        }
        write_result(id)
    }

    // Publishes every line comment sent with a freshly created review.
    async fn write_review_comments(
        &self,
        review: &str,
        patch_id: radicle::cob::ObjectId,
        comments: Vec<(String, Option<radicle::cob::CodeLocation>)>,
    ) -> Result<(), ServiceError> {
        let review = patch::ReviewId::from(parse_entry_string(review)?);
        for (index, (body, location)) in comments.into_iter().enumerate() {
            let expected = body.clone();
            let _id = self
                .mutate(
                    "create_review_comment",
                    format!("create_review_comment:{review}:{index}"),
                    move |repository, signer, marker| {
                        let mut patches = patch::Cache::no_cache(repository, signer)?;
                        let mut patch = patches.get_mut(&patch_id)?;
                        patch
                            .transaction(marker, |tx| {
                                tx.review_comment(review, body, location, None, Vec::new())
                            })
                            .map_err(PublishError::from)
                    },
                    move |repository, _author, _operation, _validation| {
                        let patch = Patches::open(repository, ReadOnly)?.get(&patch_id)?;
                        Ok(patch
                            .as_ref()
                            .and_then(|patch| find_review(patch, review))
                            .is_some_and(|value| {
                                value
                                    .comments()
                                    .any(|(_id, comment)| comment.body() == expected)
                            }))
                    },
                )
                .await?;
        }
        Ok(())
    }

    pub(super) async fn merge_patch_write(
        &self,
        request: MergePatchRequest,
    ) -> Result<Json<Document>, ServiceError> {
        let request_id = mutation_identity("merge_patch", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let revision = patch::RevisionId::from(parse_entry_string(&request.revision)?);
        let commit = radicle::git::Oid::from_str(&request.commit)
            .map_err(|error| ServiceError::Request(error.to_string()))?;
        let id = self
            .mutate(
                "merge_patch",
                request_id,
                move |repository, signer, marker| {
                    let mut patches = patch::Cache::no_cache(repository, signer)?;
                    let mut patch = patches.get_mut(&patch_id)?;
                    validate_merge(repository, &patch, signer, revision, commit)
                        .map_err(|error| PublishError::Rejected(error.to_string()))?;
                    let id = patch.transaction(marker, |tx| tx.merge(revision, commit))?;
                    patch
                        .merges()
                        .any(|(author, merge)| {
                            author == signer.public_key()
                                && merge.revision == revision
                                && merge.commit == commit
                        })
                        .then_some(id)
                        .ok_or_else(|| {
                            PublishError::Rejected("merge attestation was not applied".to_owned())
                        })
                },
                move |repository, author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return merge_operation(repository, patch_id, operation, revision, commit);
                    }
                    let patch = Patches::open(repository, ReadOnly)?
                        .get(&patch_id)?
                        .ok_or_else(|| PublishError::Rejected("patch was not found".to_owned()))?;
                    Ok(patch.merges().any(|(candidate, merge)| {
                        candidate == &author && merge.revision == revision && merge.commit == commit
                    }))
                },
            )
            .await?;
        write_result(id)
    }
}

// Rejects comment requests that no operation could satisfy.
fn validate_comment_request(
    operation: CommentOperation,
    target: CommentTargetKind,
    body: Option<&str>,
    comment: Option<EntryId>,
    embeds_empty: bool,
    reply: bool,
) -> Result<(), ServiceError> {
    let problem = match operation {
        CommentOperation::Create if matches!(target, CommentTargetKind::Review) && !reply => {
            Some("a new review comment is published with its review; pass it to create_review")
        }
        CommentOperation::Create | CommentOperation::Edit if body.is_none() => {
            Some("body is required")
        }
        CommentOperation::Edit | CommentOperation::Redact if comment.is_none() => {
            Some("comment is required")
        }
        CommentOperation::Redact if !embeds_empty => {
            Some("redacted comments cannot include embeds")
        }
        CommentOperation::Create | CommentOperation::Edit | CommentOperation::Redact => None,
    };
    match problem {
        Some(problem) => Err(ServiceError::Request(problem.to_owned())),
        None => Ok(()),
    }
}
