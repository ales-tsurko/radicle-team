//! Radicle mutations backing MCP tools.

use std::str::FromStr as _;

use radicle::{
    cob::{
        EntryId, ObjectId,
        issue::{self, Issues},
        patch::{self, Patches},
        store::access::ReadOnly,
    },
    storage::ReadRepository as _,
};
use rmcp::Json;

use crate::mcp::{
    protocol::{
        CommentOperation, CommentTargetKind, CommentWriteRequest, CreateIssueRequest,
        CreatePatchRequest, Document, EditIssueRequest, EditPatchRequest, IssueUpdate,
        MergePatchRequest, PatchCommitAction, PatchCommits, PatchUpdate, ResolveCommentRequest,
        ReviewWriteRequest, SetStateRequest, SetStringsRequest, StringUpdate, SubjectKind,
        SubjectRequest, SubjectUpdate, UpdatePatchRequest, import_commits, mutation_identity,
        parse_dids, parse_entry_string, parse_issue_state, parse_labels, parse_location,
        parse_object_string, parse_patch_commits, parse_patch_state, parse_review, parse_revision,
        parse_subject_request, parse_verdict, require_kind, stringify, validate_code_location,
        validate_merge, write_result,
    },
    recovery::{
        CommentRecovery, comment_applied, comment_operation, find_review, issue_create_operation,
        issue_edit_operation, issue_update_operation, merge_operation, patch_commit_operation,
        patch_edit_operation, patch_update_operation, review_mutation_operation,
        review_resolution_operation,
    },
    tools::ToolService,
};

impl ToolService {
    async fn mutate<T, F, V>(
        &self,
        method: &'static str,
        request: String,
        operation: F,
        validate: V,
    ) -> Result<String, String>
    where
        T: Send + ToString + 'static,
        F: FnOnce(
                &radicle::storage::git::Repository,
                &radicle::node::device::BoxedDevice,
                &str,
            ) -> Result<T, String>
            + Send
            + 'static,
        V: Fn(
                &radicle::storage::git::Repository,
                radicle::crypto::PublicKey,
                EntryId,
                crate::agent::MutationValidation,
            ) -> Result<bool, String>
            + Send
            + 'static,
    {
        let mutation = if let Some(delivery) = self.mutation_delivery {
            let invocations = self
                .mutation_invocations
                .as_ref()
                .expect("delivery invocation counters should be configured");
            let mut invocations = invocations.lock().await;
            let invocation = invocations.entry(request.clone()).or_default();
            let key = crate::agent::MutationKey {
                delivery,
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
    }

    pub(super) async fn write_comment(
        &self,
        request: CommentWriteRequest,
        operation: CommentOperation,
    ) -> Result<Json<Document>, String> {
        let method = match operation {
            CommentOperation::Create => "create_comment",
            CommentOperation::Edit => "edit_comment",
            CommentOperation::Redact => "redact_comment",
        };
        let request_id = mutation_identity(method, &request)?;
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
        if matches!(operation, CommentOperation::Create | CommentOperation::Edit) && body.is_none()
        {
            return Err("body is required".to_owned());
        }
        if matches!(operation, CommentOperation::Edit | CommentOperation::Redact)
            && comment.is_none()
        {
            return Err("comment is required".to_owned());
        }
        let kind = request.subject.kind;
        let target = request.target;
        let expected_body = body.clone();
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| match (kind, target) {
                    (SubjectKind::Issue, CommentTargetKind::Issue) => {
                        let mut issues =
                            issue::Cache::no_cache(repository, signer).map_err(stringify)?;
                        let mut issue = issues.get_mut(&subject).map_err(stringify)?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && issue.thread().comment(&comment).is_none()
                        {
                            return Err("comment was not found".to_owned());
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && issue.thread().comment(&parent).is_none()
                        {
                            return Err("reply parent was not found".to_owned());
                        }
                        issue
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.comment(
                                    body.expect("body was validated"),
                                    reply_to.unwrap_or(*subject),
                                    Vec::new(),
                                ),
                                CommentOperation::Edit => tx.edit_comment(
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    Vec::new(),
                                ),
                                CommentOperation::Redact => {
                                    tx.redact_comment(comment.expect("comment was validated"))
                                }
                            })
                            .map_err(stringify)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Revision) => {
                        let revision = parse_revision(
                            entity.ok_or_else(|| "entity revision is required".to_owned())?,
                        )?;
                        let mut patches =
                            patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                        let mut patch = patches.get_mut(&subject).map_err(stringify)?;
                        let revision_value = patch
                            .revision(&revision)
                            .ok_or_else(|| "revision was not found".to_owned())?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && revision_value.discussion().comment(&comment).is_none()
                        {
                            return Err("comment was not found".to_owned());
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && revision_value.discussion().comment(&parent).is_none()
                        {
                            return Err("reply parent was not found".to_owned());
                        }
                        if let Some(location) = location.as_ref() {
                            validate_code_location(repository, revision_value, location)?;
                        }
                        patch
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.comment(
                                    revision,
                                    body.expect("body was validated"),
                                    reply_to,
                                    location,
                                    Vec::new(),
                                ),
                                CommentOperation::Edit => tx.comment_edit(
                                    revision,
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    Vec::new(),
                                ),
                                CommentOperation::Redact => tx.comment_redact(
                                    revision,
                                    comment.expect("comment was validated"),
                                ),
                            })
                            .map_err(stringify)
                    }
                    (SubjectKind::Patch, CommentTargetKind::Review) => {
                        let review = parse_review(
                            entity.ok_or_else(|| "entity review is required".to_owned())?,
                        )?;
                        let mut patches =
                            patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                        let mut patch = patches.get_mut(&subject).map_err(stringify)?;
                        let review_value = patch
                            .revisions()
                            .flat_map(|(_revision, value)| value.reviews())
                            .find_map(|(_author, candidate)| {
                                (candidate.id() == review).then_some(candidate)
                            })
                            .ok_or_else(|| "review was not found".to_owned())?;
                        if let Some(comment) = comment
                            && !matches!(operation, CommentOperation::Create)
                            && !review_value
                                .comments()
                                .any(|(candidate, _value)| *candidate == comment)
                        {
                            return Err("comment was not found".to_owned());
                        }
                        if matches!(operation, CommentOperation::Create)
                            && let Some(parent) = reply_to
                            && !review_value
                                .comments()
                                .any(|(candidate, _value)| *candidate == parent)
                        {
                            return Err("reply parent was not found".to_owned());
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
                                .ok_or_else(|| "review was not found".to_owned())?;
                            validate_code_location(repository, revision, location)?;
                        }
                        patch
                            .transaction(marker, |tx| match operation {
                                CommentOperation::Create => tx.review_comment(
                                    review,
                                    body.expect("body was validated"),
                                    location,
                                    reply_to,
                                    Vec::new(),
                                ),
                                CommentOperation::Edit => tx.edit_review_comment(
                                    review,
                                    comment.expect("comment was validated"),
                                    body.expect("body was validated"),
                                    Vec::new(),
                                ),
                                CommentOperation::Redact => tx.redact_review_comment(
                                    review,
                                    comment.expect("comment was validated"),
                                ),
                            })
                            .map_err(stringify)
                    }
                    _ => Err("comment target does not match subject".to_owned()),
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
                    };
                    match validation {
                        crate::agent::MutationValidation::Fresh => {
                            comment_applied(repository, recovery)
                        }
                        crate::agent::MutationValidation::Recovery => {
                            comment_operation(repository, recovered, recovery)
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
    ) -> Result<Json<Document>, String> {
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
                    let mut patches =
                        patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                    let mut patch = patches.get_mut(&patch_id).map_err(stringify)?;
                    patch
                        .transaction(marker, |tx| {
                            if resolved {
                                tx.review_comment_resolve(review, comment)
                            } else {
                                tx.review_comment_unresolve(review, comment)
                            }
                        })
                        .map_err(stringify)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return review_resolution_operation(
                            repository, patch_id, operation, review, comment, resolved,
                        );
                    }
                    let patch = Patches::open(repository, ReadOnly)
                        .map_err(stringify)?
                        .get(&patch_id)
                        .map_err(stringify)?;
                    Ok(patch.is_some_and(|patch| {
                        patch
                            .revisions()
                            .flat_map(|(_revision, value)| value.reviews())
                            .find_map(|(_author, candidate)| {
                                (candidate.id() == review).then_some(candidate)
                            })
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

    pub(super) async fn create_issue_write(
        &self,
        request: CreateIssueRequest,
    ) -> Result<Json<Document>, String> {
        let request_id = mutation_identity("create_issue", &request)?;
        let title = radicle::cob::Title::try_from(request.title).map_err(stringify)?;
        let labels = parse_labels(request.labels)?;
        let assignees = parse_dids(request.assignees)?;
        let description = request.description;
        let expected_title = title.clone();
        let expected_labels = labels.clone();
        let expected_assignees = assignees.clone();
        let expected_description = description.clone();
        let id = self
            .mutate(
                "create_issue",
                request_id,
                move |repository, signer, marker| {
                    let identity = repository.identity_head().map_err(stringify)?;
                    let mut issues = radicle::cob::store::Store::<issue::Issue, _, _>::open(
                        repository,
                        radicle::cob::store::access::WriteAs::new(signer),
                    )
                    .map_err(stringify)?
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
                    )
                    .map_err(stringify)?;
                    Ok(id)
                },
                move |repository, _author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return issue_create_operation(
                            repository,
                            operation,
                            &expected_title,
                            &expected_description,
                            &expected_labels,
                            &expected_assignees,
                        );
                    }
                    Issues::open(repository, ReadOnly)
                        .map_err(stringify)?
                        .get(&ObjectId::from(operation))
                        .map(|issue| issue.is_some())
                        .map_err(stringify)
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn edit_issue_write(
        &self,
        request: EditIssueRequest,
    ) -> Result<Json<Document>, String> {
        let request_id = mutation_identity("edit_issue", &request)?;
        require_kind(request.subject.kind, SubjectKind::Issue)?;
        let issue_id = parse_object_string(&request.subject.subject)?;
        let title = request
            .title
            .map(radicle::cob::Title::try_from)
            .transpose()
            .map_err(stringify)?;
        let description = request.description;
        let expected_title = title.clone();
        let expected_description = description.clone();
        if title.is_none() && description.is_none() {
            return Err("title or description is required".to_owned());
        }
        let id = self
            .mutate(
                "edit_issue",
                request_id,
                move |repository, signer, marker| {
                    let mut issues =
                        issue::Cache::no_cache(repository, signer).map_err(stringify)?;
                    let mut issue = issues.get_mut(&issue_id).map_err(stringify)?;
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
                        .map_err(stringify)
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
    ) -> Result<Json<Document>, String> {
        let request_id = mutation_identity("create_patch", &request)?;
        let title = radicle::cob::Title::try_from(request.title).map_err(stringify)?;
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
    ) -> Result<Json<Document>, String> {
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
    ) -> Result<Json<Document>, String> {
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
                    )?;
                    match action {
                        PatchCommitAction::Create {
                            title,
                            labels,
                            draft,
                        } => {
                            let identity = repository.identity_head().map_err(stringify)?;
                            let mut patches =
                                radicle::cob::store::Store::<patch::Patch, _, _>::open(
                                    repository,
                                    radicle::cob::store::access::WriteAs::new(signer),
                                )
                                .map_err(stringify)?
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
                            )
                            .map_err(stringify)?;
                            Ok(id.to_string())
                        }
                        PatchCommitAction::Update(id) => {
                            let mut patches =
                                patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                            let mut patch = patches.get_mut(&id).map_err(stringify)?;
                            patch
                                .transaction(marker, |tx| {
                                    tx.revision(commits.description, commits.base, commits.head)
                                })
                                .map(|id| id.to_string())
                                .map_err(stringify)
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
                    let patches = Patches::open(repository, ReadOnly).map_err(stringify)?;
                    match expected_action {
                        PatchCommitAction::Create { .. } => patches
                            .get(&ObjectId::from(operation))
                            .map(|patch| patch.is_some())
                            .map_err(stringify),
                        PatchCommitAction::Update(id) => {
                            let patch = patches.get(&id).map_err(stringify)?;
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
    ) -> Result<Json<Document>, String> {
        let request_id = mutation_identity("edit_patch", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let title = request
            .title
            .map(radicle::cob::Title::try_from)
            .transpose()
            .map_err(stringify)?;
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
            return Err("title or description is required".to_owned());
        }
        if description.is_some() && revision.is_none() {
            return Err("revision is required when editing a description".to_owned());
        }
        let id = self
            .mutate(
                "edit_patch",
                request_id,
                move |repository, signer, marker| {
                    let mut patches =
                        patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                    let mut patch = patches.get_mut(&patch_id).map_err(stringify)?;
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
                        .map_err(stringify)
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
                    let patch = Patches::open(repository, ReadOnly)
                        .map_err(stringify)?
                        .get(&patch_id)
                        .map_err(stringify)?;
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
    ) -> Result<Json<Document>, String> {
        let expected_subject = subject.clone();
        let expected_update = update.clone();
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| match update {
                    SubjectUpdate::Issue(update) => {
                        let id = parse_subject_request(&subject, SubjectKind::Issue)?;
                        let mut issues =
                            issue::Cache::no_cache(repository, signer).map_err(stringify)?;
                        let mut issue = issues.get_mut(&id).map_err(stringify)?;
                        issue
                            .transaction(marker, |tx| match update {
                                IssueUpdate::State(state) => tx.lifecycle(state),
                                IssueUpdate::Labels(labels) => tx.label(labels),
                                IssueUpdate::Assignees(assignees) => tx.assign(assignees),
                            })
                            .map_err(stringify)
                    }
                    SubjectUpdate::Patch(update) => {
                        let id = parse_subject_request(&subject, SubjectKind::Patch)?;
                        let mut patches =
                            patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                        let mut patch = patches.get_mut(&id).map_err(stringify)?;
                        patch
                            .transaction(marker, |tx| match update {
                                PatchUpdate::State(state) => tx.lifecycle(state),
                                PatchUpdate::Labels(labels) => tx.label(labels),
                                PatchUpdate::Assignees(assignees) => tx.assign(assignees),
                            })
                            .map_err(stringify)
                    }
                },
                move |repository, _author, operation, validation| match &expected_update {
                    SubjectUpdate::Issue(update) => {
                        if matches!(validation, crate::agent::MutationValidation::Recovery) {
                            let id = parse_subject_request(&expected_subject, SubjectKind::Issue)?;
                            return issue_update_operation(repository, id, operation, update);
                        }
                        let id = parse_subject_request(&expected_subject, SubjectKind::Issue)?;
                        let issue = Issues::open(repository, ReadOnly)
                            .map_err(stringify)?
                            .get(&id)
                            .map_err(stringify)?;
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
                            let id = parse_subject_request(&expected_subject, SubjectKind::Patch)?;
                            return patch_update_operation(repository, id, operation, update);
                        }
                        let id = parse_subject_request(&expected_subject, SubjectKind::Patch)?;
                        let patch = Patches::open(repository, ReadOnly)
                            .map_err(stringify)?
                            .get(&id)
                            .map_err(stringify)?;
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
    ) -> Result<Json<Document>, String> {
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
    ) -> Result<Json<Document>, String> {
        let method = match (kind, change) {
            (SubjectKind::Issue, StringUpdate::Labels) => "set_issue_labels",
            (SubjectKind::Issue, StringUpdate::Assignees) => "set_issue_assignees",
            (SubjectKind::Patch, StringUpdate::Labels) => "set_patch_labels",
            (SubjectKind::Patch, StringUpdate::Assignees) => "set_patch_assignees",
        };
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
    ) -> Result<Json<Document>, String> {
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
        let expected_review = review;
        let expected_verdict = verdict;
        let expected_summary = summary.clone();
        let expected_labels = labels.clone();
        if edit && review.is_none() {
            return Err("review is required".to_owned());
        }
        if edit && summary.is_none() {
            return Err("summary is required".to_owned());
        }
        if !edit && revision.is_none() {
            return Err("revision is required".to_owned());
        }
        if !edit && verdict.is_none() && summary.is_none() {
            return Err("verdict or summary is required".to_owned());
        }
        let id = self
            .mutate(
                method,
                request_id,
                move |repository, signer, marker| {
                    let mut patches =
                        patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                    let mut patch = patches.get_mut(&patch_id).map_err(stringify)?;
                    if edit {
                        let review = review.expect("review was validated");
                        let review = find_review(&patch, review)
                            .ok_or_else(|| "review was not found".to_owned())?;
                        if review.author().public_key() != signer.public_key() {
                            return Err("review belongs to another author".to_owned());
                        }
                    } else {
                        let revision = revision.expect("revision was validated");
                        let revision = patch
                            .revision(&revision)
                            .ok_or_else(|| "revision was not found".to_owned())?;
                        if revision
                            .reviews()
                            .any(|(author, _review)| author == signer.public_key())
                        {
                            return Err("revision was already reviewed by this author".to_owned());
                        }
                    }
                    patch
                        .transaction(marker, |tx| {
                            if edit {
                                tx.review_edit(
                                    review.expect("review was validated"),
                                    verdict,
                                    summary.expect("summary was validated"),
                                    labels,
                                    Vec::new(),
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
                        .map_err(stringify)
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
                        );
                    }
                    let patch = Patches::open(repository, ReadOnly)
                        .map_err(stringify)?
                        .get(&patch_id)
                        .map_err(stringify)?;
                    let id = if edit {
                        expected_review.expect("review was validated")
                    } else {
                        patch::ReviewId::from(operation)
                    };
                    Ok(patch
                        .as_ref()
                        .and_then(|patch| {
                            patch
                                .revisions()
                                .flat_map(|(_revision, value)| value.reviews())
                                .find_map(|(_author, review)| (review.id() == id).then_some(review))
                        })
                        .is_some_and(|review| {
                            review.verdict() == expected_verdict
                                && review.summary()
                                    == expected_summary.as_deref().unwrap_or_default()
                                && review.labels().count() == expected_labels.len()
                                && expected_labels
                                    .iter()
                                    .all(|label| review.labels().any(|value| value == label))
                        }))
                },
            )
            .await?;
        write_result(id)
    }

    pub(super) async fn merge_patch_write(
        &self,
        request: MergePatchRequest,
    ) -> Result<Json<Document>, String> {
        let request_id = mutation_identity("merge_patch", &request)?;
        require_kind(request.subject.kind, SubjectKind::Patch)?;
        let patch_id = parse_object_string(&request.subject.subject)?;
        let revision = patch::RevisionId::from(parse_entry_string(&request.revision)?);
        let commit = radicle::git::Oid::from_str(&request.commit).map_err(stringify)?;
        let id = self
            .mutate(
                "merge_patch",
                request_id,
                move |repository, signer, marker| {
                    let mut patches =
                        patch::Cache::no_cache(repository, signer).map_err(stringify)?;
                    let mut patch = patches.get_mut(&patch_id).map_err(stringify)?;
                    validate_merge(repository, &patch, signer, revision, commit)?;
                    let id = patch
                        .transaction(marker, |tx| tx.merge(revision, commit))
                        .map_err(stringify)?;
                    patch
                        .merges()
                        .any(|(author, merge)| {
                            author == signer.public_key()
                                && merge.revision == revision
                                && merge.commit == commit
                        })
                        .then_some(id)
                        .ok_or_else(|| "merge attestation was not applied".to_owned())
                },
                move |repository, author, operation, validation| {
                    if matches!(validation, crate::agent::MutationValidation::Recovery) {
                        return merge_operation(repository, patch_id, operation, revision, commit);
                    }
                    let patch = Patches::open(repository, ReadOnly)
                        .map_err(stringify)?
                        .get(&patch_id)
                        .map_err(stringify)?
                        .ok_or_else(|| "patch was not found".to_owned())?;
                    Ok(patch.merges().any(|(candidate, merge)| {
                        candidate == &author && merge.revision == revision && merge.commit == commit
                    }))
                },
            )
            .await?;
        write_result(id)
    }
}
