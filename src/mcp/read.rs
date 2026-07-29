//! Radicle reads backing MCP tools.

use std::str::FromStr as _;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use radicle::{
    cob::{
        Embed, EntryId, ObjectId, Op,
        common::Uri,
        issue::{Action as IssueAction, IssueStream, Issues, TYPENAME as ISSUE_TYPENAME},
        patch::{self, Action as PatchAction, PatchStream, Patches, TYPENAME as PATCH_TYPENAME},
        store::access::ReadOnly,
        stream::CobStream,
    },
    storage::ReadStorage as _,
};
use serde::Serialize;
use serde_json::Value;

use crate::mcp::{
    protocol::{
        DiffDocument, DiffFile, DiffLine, DiffRequest, EmbedDocument, EmbedReadRequest,
        EntityRequest, EntryRequest, IssuePage, IssueRequest, IssueStateFilter, ListIssuesRequest,
        MAX_EMBED_BYTES, PatchEntity, PatchEntityRequest, SubjectDocument, SubjectKind,
        SubjectRequest, ThreadKind, TimelinePage, TimelineRequest, parse_entry, parse_object,
    },
    server::ServiceError,
    tools::ToolService,
};

pub(super) const MAX_DIFF_BYTES: usize = 1_000_000;
const MAX_DIFF_LINES: usize = 10_000;

macro_rules! page_reader {
    ($name:ident, $action:ty, $kind:expr, $stream:ident, $typename:expr) => {
        fn $name(
            &self,
            subject: &SubjectRequest,
            cursor: Option<&str>,
            limit: Option<u32>,
            predicate: impl Fn(&Op<$action>) -> bool,
        ) -> Result<TimelinePage, ServiceError> {
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
        ) -> Result<Value, ServiceError> {
            let (repository, id) = self.operation_source(subject, $kind)?;
            find_operation($stream::init(id, &repository).all()?, entry, requested)
        }
    };
}

impl ToolService {
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

    pub(super) async fn blocking<T, F>(&self, operation: F) -> Result<T, ServiceError>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Result<T, ServiceError> + Send + 'static,
    {
        let service = self.clone();
        tokio::task::spawn_blocking(move || operation(service))
            .await
            .map_err(|error| ServiceError::Task(error.to_string()))?
    }

    fn repository(&self) -> Result<radicle::storage::git::Repository, ServiceError> {
        self.storage
            .repository(self.repository)
            .map_err(ServiceError::Repository)
    }

    pub(super) fn subject(&self, request: SubjectRequest) -> Result<Value, ServiceError> {
        let document = match request.kind {
            SubjectKind::Issue => self.issue_subject(request),
            SubjectKind::Patch => self.patch_subject(request),
        }?;
        serialize(&document)
    }

    pub(super) fn issue(&self, request: IssueRequest) -> Result<Value, ServiceError> {
        serialize(&self.issue_subject(SubjectRequest {
            kind: SubjectKind::Issue,
            subject: request.issue,
        })?)
    }

    pub(super) fn issues(&self, request: ListIssuesRequest) -> Result<IssuePage, ServiceError> {
        let repository = self.repository()?;
        let issues = Issues::open(&repository, ReadOnly)?;
        let query = request.query.map(|query| query.to_lowercase());
        let mut documents = issues
            .all()?
            .map(|result| {
                let (id, issue) = result?;
                Ok(SubjectDocument::issue(id.to_string(), &issue))
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        documents.retain(|issue| {
            let state_matches = request.state.is_none_or(|state| match state {
                IssueStateFilter::Open => issue.state == "open",
                IssueStateFilter::Closed => issue.state == "closed",
            });
            let query_matches = query.as_ref().is_none_or(|query| {
                issue.title.to_lowercase().contains(query)
                    || issue.description.to_lowercase().contains(query)
            });
            state_matches && query_matches
        });
        documents.sort_unstable_by(|left, right| left.subject.cmp(&right.subject));

        let start = match request.cursor.as_deref() {
            Some(cursor) => documents
                .iter()
                .position(|issue| issue.subject == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| ServiceError::InvalidCursor(cursor.to_owned()))?,
            None => 0,
        };
        let limit = request
            .limit
            .map_or(DEFAULT_PAGE_SIZE, |limit| limit as usize)
            .clamp(1, MAX_PAGE_SIZE);
        let end = start.saturating_add(limit).min(documents.len());
        let next_cursor = (end < documents.len())
            .then(|| documents.get(end.saturating_sub(1)))
            .flatten()
            .map(|issue| issue.subject.clone());

        Ok(IssuePage {
            issues: documents.drain(start..end).collect(),
            next_cursor,
        })
    }

    fn issue_subject(&self, request: SubjectRequest) -> Result<SubjectDocument, ServiceError> {
        let (repository, id) = self.operation_source(&request, SubjectKind::Issue)?;
        let issue = Issues::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or_else(|| ServiceError::NotFound(request.subject.clone()))?;
        Ok(SubjectDocument::issue(request.subject, &issue))
    }

    fn patch_subject(&self, request: SubjectRequest) -> Result<SubjectDocument, ServiceError> {
        let (repository, id) = self.operation_source(&request, SubjectKind::Patch)?;
        let patch = Patches::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or_else(|| ServiceError::NotFound(request.subject.clone()))?;
        Ok(SubjectDocument::patch(request.subject, &patch))
    }

    pub(super) fn entry(&self, request: EntryRequest) -> Result<Value, ServiceError> {
        let entry = parse_entry(&request.entry)?;
        match request.subject.kind {
            SubjectKind::Issue => self.issue_entry(&request.subject, entry, request.entry),
            SubjectKind::Patch => self.patch_entry(&request.subject, entry, request.entry),
        }
    }

    pub(super) fn timeline(&self, request: TimelineRequest) -> Result<TimelinePage, ServiceError> {
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

    pub(super) fn thread(&self, request: EntityRequest) -> Result<TimelinePage, ServiceError> {
        match request.thread {
            ThreadKind::Issue => self.issue_thread(&request),
            ThreadKind::Review => self.review_thread(&request),
            ThreadKind::Revision => self.revision_thread(&request),
        }
    }

    fn issue_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ServiceError> {
        if request.subject.kind != SubjectKind::Issue || request.entity.is_some() {
            return Err(ServiceError::InvalidThread);
        }
        self.issue_page(
            &request.subject,
            request.cursor.as_deref(),
            request.limit,
            |operation| operation.actions.iter().any(issue_discussion_action),
        )
    }

    fn review_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ServiceError> {
        self.patch_thread(request, review_discussion_operation)
    }

    fn revision_thread(&self, request: &EntityRequest) -> Result<TimelinePage, ServiceError> {
        self.patch_thread(request, revision_discussion_operation)
    }

    fn patch_thread(
        &self,
        request: &EntityRequest,
        predicate: impl Fn(&Op<PatchAction>, &str) -> bool,
    ) -> Result<TimelinePage, ServiceError> {
        let id = request
            .entity
            .as_deref()
            .ok_or(ServiceError::InvalidThread)?;
        self.patch_page(
            &request.subject,
            request.cursor.as_deref(),
            request.limit,
            |operation| predicate(operation, id),
        )
    }

    pub(super) fn patch_entity(
        &self,
        request: PatchEntityRequest,
        entity: PatchEntity,
    ) -> Result<TimelinePage, ServiceError> {
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
            Err(ServiceError::NotFound(request.entity))
        } else {
            Ok(page)
        }
    }

    fn operation_source(
        &self,
        subject: &SubjectRequest,
        expected: SubjectKind,
    ) -> Result<(radicle::storage::git::Repository, ObjectId), ServiceError> {
        if subject.kind != expected {
            return Err(ServiceError::SubjectKind);
        }
        Ok((self.repository()?, parse_object(&subject.subject)?))
    }

    pub(super) fn diff(&self, request: DiffRequest) -> Result<DiffDocument, ServiceError> {
        if request.subject.kind != SubjectKind::Patch {
            return Err(ServiceError::SubjectKind);
        }
        let repository = self.repository()?;
        let id = parse_object(&request.subject.subject)?;
        let patch = Patches::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or_else(|| ServiceError::NotFound(request.subject.subject.clone()))?;
        let revision = match request.revision {
            Some(revision) => {
                let id = patch::RevisionId::from(
                    parse_entry(&revision)
                        .map_err(|_error| ServiceError::InvalidId(revision.clone()))?,
                );
                patch
                    .revision(&id)
                    .ok_or(ServiceError::NotFound(revision))?
            }
            None => patch.latest().1,
        };
        let base = repository.backend.find_commit((*revision.base()).into())?;
        let head = repository.backend.find_commit(revision.head().into())?;
        let diff =
            repository
                .backend
                .diff_tree_to_tree(Some(&base.tree()?), Some(&head.tree()?), None)?;
        let mut files: Vec<DiffFile> = Vec::new();
        let mut bytes = 0usize;
        let mut lines = 0usize;
        let mut limit_exceeded = false;
        let mut current_paths: Option<(Option<std::path::PathBuf>, Option<std::path::PathBuf>)> =
            None;
        let printed = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            let old_path = delta.old_file().path();
            let new_path = delta.new_file().path();
            let new_file = current_paths
                .as_ref()
                .is_none_or(|(old, new)| old.as_deref() != old_path || new.as_deref() != new_path);
            let path_bytes = if new_file {
                old_path.map_or(0, |path| path.as_os_str().len())
                    + new_path.map_or(0, |path| path.as_os_str().len())
            } else {
                0
            };
            let added = line.content().len().saturating_add(path_bytes);
            if lines >= MAX_DIFF_LINES
                || bytes
                    .checked_add(added)
                    .is_none_or(|total| total > MAX_DIFF_BYTES)
            {
                limit_exceeded = true;
                return false;
            }
            bytes += added;
            lines += 1;
            if new_file {
                current_paths = Some((
                    old_path.map(std::path::Path::to_path_buf),
                    new_path.map(std::path::Path::to_path_buf),
                ));
                files.push(DiffFile {
                    old_path: old_path.map(|path| path.display().to_string()),
                    new_path: new_path.map(|path| path.display().to_string()),
                    lines: Vec::new(),
                });
            }
            if let Some(file) = files.last_mut() {
                file.lines.push(DiffLine {
                    origin: line.origin(),
                    old_line: line.old_lineno(),
                    new_line: line.new_lineno(),
                    content: String::from_utf8_lossy(line.content()).into_owned(),
                });
            }
            true
        });
        if limit_exceeded {
            return Err(ServiceError::DiffLimit);
        }
        printed?;
        Ok(DiffDocument {
            base: revision.base().to_string(),
            head: revision.head().to_string(),
            files,
        })
    }

    pub(super) fn embed(&self, request: EmbedReadRequest) -> Result<EmbedDocument, ServiceError> {
        let oid = request.oid()?;
        let (repository, subject) =
            self.operation_source(&request.subject, request.subject.kind)?;
        let reference = EmbedReference(Uri::from(oid));
        let referenced = match request.subject.kind {
            SubjectKind::Issue => {
                reference.exists_in_issues(IssueStream::init(subject, &repository).all()?)?
            }
            SubjectKind::Patch => {
                reference.exists_in_patches(PatchStream::init(subject, &repository).all()?)?
            }
        };
        if !referenced {
            return Err(ServiceError::NotFound(request.content));
        }
        let raw_oid = oid.into();
        let (size, kind) = repository.backend.odb()?.read_header(raw_oid)?;
        if kind != git2::ObjectType::Blob {
            return Err(ServiceError::NotFound(request.content));
        }
        if size > MAX_EMBED_BYTES {
            return Err(ServiceError::EmbedLimit);
        }
        let blob = repository.backend.find_blob(raw_oid)?;
        Ok(EmbedDocument {
            content_base64: BASE64_STANDARD.encode(blob.content()),
        })
    }
}

struct EmbedReference(Uri);

impl EmbedReference {
    fn exists_in_issues(
        &self,
        operations: impl Iterator<Item = Result<Op<IssueAction>, radicle::cob::stream::error::Ops>>,
    ) -> Result<bool, ServiceError> {
        self.exists_in(operations, |action| match action {
            IssueAction::Comment { embeds, .. } | IssueAction::CommentEdit { embeds, .. } => embeds,
            _ => &[],
        })
    }

    fn exists_in_patches(
        &self,
        operations: impl Iterator<Item = Result<Op<PatchAction>, radicle::cob::stream::error::Ops>>,
    ) -> Result<bool, ServiceError> {
        self.exists_in(operations, |action| match action {
            PatchAction::ReviewComment { embeds, .. }
            | PatchAction::ReviewCommentEdit { embeds, .. }
            | PatchAction::RevisionComment { embeds, .. }
            | PatchAction::RevisionCommentEdit { embeds, .. }
            | PatchAction::RevisionEdit { embeds, .. } => embeds,
            PatchAction::ReviewEdit(edit) => edit.embeds().map(Vec::as_slice).unwrap_or_default(),
            _ => &[],
        })
    }

    fn exists_in<A>(
        &self,
        operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
        embeds: for<'a> fn(&'a A) -> &'a [Embed<Uri>],
    ) -> Result<bool, ServiceError> {
        for operation in operations {
            if operation
                .map_err(ServiceError::Operation)?
                .actions
                .iter()
                .any(|action| embeds(action).iter().any(|embed| embed.content == self.0))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 200;

pub(super) fn find_operation<A: Serialize>(
    operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
    entry: EntryId,
    requested: String,
) -> Result<Value, ServiceError> {
    for operation in operations {
        let operation = operation.map_err(ServiceError::Operation)?;
        if operation.id == entry {
            return serialize(&operation);
        }
    }
    Err(ServiceError::NotFound(requested))
}

pub(super) fn operation_page<S>(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    stream: &S,
    cursor: Option<&str>,
    limit: Option<u32>,
    predicate: impl Fn(&Op<S::Action>) -> bool,
) -> Result<TimelinePage, ServiceError>
where
    S: CobStream<IterError = radicle::cob::stream::error::Ops>,
    S::Action: Serialize,
{
    let cursor = parse_cursor(cursor)?;
    validate_cursor(repository, object, typename, cursor)?;
    let operations = resume_stream(stream, cursor)?;
    paginate_operations(operations, cursor, limit, predicate)
}

fn resume_stream<S>(stream: &S, cursor: Option<EntryId>) -> Result<S::Iter, ServiceError>
where
    S: CobStream<IterError = radicle::cob::stream::error::Ops>,
{
    match cursor {
        Some(cursor) => stream.since(cursor).map_err(ServiceError::from),
        None => stream.all().map_err(ServiceError::from),
    }
}

pub(super) fn paginate_operations<A: Serialize>(
    operations: impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
    cursor: Option<EntryId>,
    limit: Option<u32>,
    predicate: impl Fn(&Op<A>) -> bool,
) -> Result<TimelinePage, ServiceError> {
    let mut operations = operations.into_iter();
    if let Some(cursor) = cursor {
        match operations.next() {
            Some(Ok(operation)) if operation.id == cursor && predicate(&operation) => {}
            _ => return Err(ServiceError::InvalidCursor(cursor.to_string())),
        }
    }
    let limit = limit
        .map_or(DEFAULT_PAGE_SIZE, |limit| limit as usize)
        .clamp(1, MAX_PAGE_SIZE);
    let mut entries = Vec::with_capacity(limit);
    let mut last = None;
    for operation in operations {
        let operation = operation.map_err(ServiceError::Operation)?;
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

fn parse_cursor(cursor: Option<&str>) -> Result<Option<EntryId>, ServiceError> {
    cursor
        .map(|cursor| {
            EntryId::from_str(cursor)
                .map_err(|_error| ServiceError::InvalidCursor(cursor.to_owned()))
        })
        .transpose()
}

fn validate_cursor(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    cursor: Option<EntryId>,
) -> Result<(), ServiceError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor_reachable(repository, object, typename, cursor)? {
        Ok(())
    } else {
        Err(ServiceError::InvalidCursor(cursor.to_string()))
    }
}

fn cursor_reachable(
    repository: &radicle::storage::git::Repository,
    object: ObjectId,
    typename: &radicle::cob::TypeName,
    cursor: EntryId,
) -> Result<bool, ServiceError> {
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
) -> Result<bool, ServiceError> {
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
        .map_err(ServiceError::from)
}

pub(super) fn issue_discussion_action(action: &IssueAction) -> bool {
    matches!(
        action,
        IssueAction::Comment { .. }
            | IssueAction::CommentEdit { .. }
            | IssueAction::CommentRedact { .. }
            | IssueAction::CommentReact { .. }
    )
}

pub(super) fn review_discussion_operation(operation: &Op<PatchAction>, id: &str) -> bool {
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

pub(super) fn revision_discussion_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    operation.actions.iter().any(|action| match action {
        PatchAction::RevisionComment { revision, .. }
        | PatchAction::RevisionCommentEdit { revision, .. }
        | PatchAction::RevisionCommentRedact { revision, .. }
        | PatchAction::RevisionCommentReact { revision, .. } => revision.to_string() == id,
        _ => false,
    })
}

pub(super) fn review_operation(operation: &Op<PatchAction>, id: &str) -> bool {
    patch_entity_operation(operation, id, PatchEntity::Review)
}

pub(super) fn revision_operation(operation: &Op<PatchAction>, id: &str) -> bool {
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

pub(super) fn revision_action(operation: EntryId, action: &PatchAction, id: &str) -> bool {
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

pub(super) fn serialize(value: &impl Serialize) -> Result<Value, ServiceError> {
    serde_json::to_value(value).map_err(ServiceError::Encoding)
}
