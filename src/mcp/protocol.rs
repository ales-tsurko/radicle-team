//! MCP request types, conversions, and validation.

use std::{collections::BTreeSet, str::FromStr as _};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use radicle::{
    cob::{Embed, EntryId, ObjectId, common::Uri, issue, patch},
    storage::ReadRepository as _,
};
use rmcp::Json;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::server::ServiceError;

pub(super) const MAX_EMBED_BYTES: usize = 10_000_000;
const MAX_EMBEDS: usize = 16;
const MAX_ENCODED_EMBED_BYTES: usize = (MAX_EMBED_BYTES + 2 * MAX_EMBEDS).div_ceil(3) * 4;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct SubjectRequest {
    pub(super) kind: SubjectKind,
    pub(super) subject: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SubjectDocument {
    pub(super) kind: SubjectKind,
    pub(super) subject: String,
    pub(super) title: String,
    pub(super) description: String,
    pub(super) state: String,
    pub(super) author: String,
    pub(super) assignees: Vec<String>,
    pub(super) labels: Vec<String>,
    pub(super) base: Option<String>,
    pub(super) head: Option<String>,
}

impl SubjectDocument {
    pub(super) fn issue(subject: String, issue: &radicle::cob::issue::Issue) -> Self {
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

    pub(super) fn patch(subject: String, patch: &radicle::cob::patch::Patch) -> Self {
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

    pub(super) fn new<A, L>(
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

pub(super) struct SubjectCore<'a> {
    pub(super) kind: SubjectKind,
    pub(super) subject: String,
    pub(super) title: &'a str,
    pub(super) description: &'a str,
    pub(super) state: String,
    pub(super) author: String,
    pub(super) commits: (Option<&'a radicle::git::Oid>, Option<&'a radicle::git::Oid>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum SubjectKind {
    Issue,
    Patch,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct EntryRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) entry: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct TimelineRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) cursor: Option<String>,
    pub(super) limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct TimelinePage {
    pub(super) entries: Vec<Value>,
    pub(super) next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ListIssuesRequest {
    pub(super) query: Option<String>,
    pub(super) state: Option<IssueStateFilter>,
    pub(super) cursor: Option<String>,
    pub(super) limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum IssueStateFilter {
    Open,
    Closed,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct IssuePage {
    pub(super) issues: Vec<SubjectDocument>,
    pub(super) next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct IssueRequest {
    pub(super) issue: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Document {
    pub(super) data: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct EntityRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) thread: ThreadKind,
    pub(super) entity: Option<String>,
    pub(super) cursor: Option<String>,
    pub(super) limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ThreadKind {
    Issue,
    Review,
    Revision,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PatchEntityRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) entity: String,
    pub(super) cursor: Option<String>,
    pub(super) limit: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum PatchEntity {
    Review,
    Revision,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct DiffRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) revision: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommentTargetKind {
    Issue,
    Revision,
    Review,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct CommentWriteRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) target: CommentTargetKind,
    pub(super) entity: Option<String>,
    pub(super) comment: Option<String>,
    pub(super) body: Option<String>,
    pub(super) reply_to: Option<String>,
    pub(super) location: Option<LocationRequest>,
    #[serde(default)]
    pub(super) embeds: Vec<EmbedWriteRequest>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CommentOperation {
    Create,
    Edit,
    Redact,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct EmbedWriteRequest {
    pub(super) name: String,
    pub(super) content_base64: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct EmbedReadRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) content: String,
}

impl EmbedReadRequest {
    pub(super) fn oid(&self) -> Result<radicle::git::Oid, ServiceError> {
        let uri = Uri::from_str(&self.content)
            .map_err(|_error| ServiceError::InvalidId(self.content.clone()))?;
        radicle::git::Oid::try_from(&uri)
            .map_err(|_error| ServiceError::InvalidId(self.content.clone()))
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct EmbedDocument {
    pub(super) content_base64: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct ReactionWriteRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) target: CommentTargetKind,
    pub(super) entity: Option<String>,
    pub(super) comment: Option<String>,
    pub(super) reaction: String,
    pub(super) active: bool,
    pub(super) location: Option<LocationRequest>,
}

impl ReactionWriteRequest {
    pub(super) fn parsed_reaction(&self) -> Result<radicle::cob::Reaction, ServiceError> {
        radicle::cob::Reaction::from_str(&self.reaction)
            .map_err(|error| ServiceError::Request(stringify(error)))
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedEmbed {
    content: Vec<u8>,
    oid: radicle::git::Oid,
    reference: Embed<Uri>,
}

impl PreparedEmbed {
    pub(super) fn prepare(requests: Vec<EmbedWriteRequest>) -> Result<Vec<Self>, ServiceError> {
        if requests.len() > MAX_EMBEDS {
            return Err(ServiceError::Request(format!(
                "at most {MAX_EMBEDS} embeds are allowed"
            )));
        }
        let encoded_bytes = requests.iter().try_fold(0usize, |total, request| {
            total
                .checked_add(request.content_base64.len())
                .ok_or_else(|| ServiceError::Request("embed size overflow".to_owned()))
        })?;
        if encoded_bytes > MAX_ENCODED_EMBED_BYTES {
            return Err(ServiceError::Request(format!(
                "combined embed content exceeds {MAX_EMBED_BYTES} bytes"
            )));
        }
        let mut bytes = 0usize;
        let mut names = BTreeSet::new();
        requests
            .into_iter()
            .map(|request| {
                if request.name.is_empty()
                    || request.name.len() > 255
                    || matches!(request.name.as_str(), "." | "..")
                    || request.name.chars().any(|character| {
                        character.is_control() || matches!(character, '/' | '\\' | ':' | '\0')
                    })
                {
                    return Err(ServiceError::Request(
                        "embed name must be a safe file name of at most 255 bytes".to_owned(),
                    ));
                }
                if !names.insert(request.name.clone()) {
                    return Err(ServiceError::Request(format!(
                        "duplicate embed name '{}'",
                        request.name
                    )));
                }
                let content = BASE64_STANDARD
                    .decode(request.content_base64)
                    .map_err(|error| {
                        ServiceError::Request(format!("invalid base64 embed content: {error}"))
                    })?;
                bytes = bytes
                    .checked_add(content.len())
                    .ok_or_else(|| ServiceError::Request("embed size overflow".to_owned()))?;
                if bytes > MAX_EMBED_BYTES {
                    return Err(ServiceError::Request(format!(
                        "combined embed content exceeds {MAX_EMBED_BYTES} bytes"
                    )));
                }
                let oid = git2::Oid::hash_object(git2::ObjectType::Blob, &content)
                    .map(radicle::git::Oid::from)
                    .map_err(|error| ServiceError::Request(stringify(error)))?;
                Ok(Self {
                    content,
                    oid,
                    reference: Embed {
                        name: request.name,
                        content: Uri::from(oid),
                    },
                })
            })
            .collect()
    }

    pub(super) fn references(embeds: &[Self]) -> Vec<Embed<Uri>> {
        embeds.iter().map(|embed| embed.reference.clone()).collect()
    }

    pub(super) fn store(
        embeds: &[Self],
        repository: &radicle::storage::git::Repository,
    ) -> Result<Vec<Embed<Uri>>, ServiceError> {
        for embed in embeds {
            let stored = repository
                .backend
                .blob(&embed.content)
                .map_err(|error| ServiceError::Request(stringify(error)))?;
            if radicle::git::Oid::from(stored) != embed.oid {
                return Err(ServiceError::Request(
                    "stored embed hash does not match its reference".to_owned(),
                ));
            }
        }
        Ok(Self::references(embeds))
    }
}

#[derive(Clone)]
pub(super) enum IssueUpdate {
    State(issue::State),
    Labels(Vec<radicle::cob::Label>),
    Assignees(Vec<radicle::identity::Did>),
}

#[derive(Clone)]
pub(super) enum PatchUpdate {
    State(patch::Lifecycle),
    Labels(Vec<radicle::cob::Label>),
    Assignees(BTreeSet<radicle::identity::Did>),
}

#[derive(Clone)]
pub(super) enum SubjectUpdate {
    Issue(IssueUpdate),
    Patch(PatchUpdate),
}

#[derive(Clone, Copy)]
pub(super) enum StringUpdate {
    Labels,
    Assignees,
}

#[derive(Clone)]
pub(super) struct PatchCommits {
    pub(super) description: String,
    pub(super) base: radicle::git::Oid,
    pub(super) head: radicle::git::Oid,
    pub(super) workspace: std::path::PathBuf,
}

#[derive(Clone)]
pub(super) enum PatchCommitAction {
    Create {
        title: radicle::cob::Title,
        labels: Vec<radicle::cob::Label>,
        draft: bool,
    },
    Update(ObjectId),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct ResolveCommentRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) review: String,
    pub(super) comment: String,
    pub(super) resolved: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct LocationRequest {
    pub(super) commit: String,
    pub(super) path: String,
    pub(super) old_start: Option<usize>,
    pub(super) old_end: Option<usize>,
    pub(super) new_start: Option<usize>,
    pub(super) new_end: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct CreateIssueRequest {
    pub(super) title: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) labels: Vec<String>,
    #[serde(default)]
    pub(super) assignees: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct CommentIssueRequest {
    pub(super) issue: String,
    pub(super) body: String,
    pub(super) reply_to: Option<String>,
    #[serde(default)]
    pub(super) embeds: Vec<EmbedWriteRequest>,
}

impl CommentIssueRequest {
    pub(super) fn comment(self) -> CommentWriteRequest {
        CommentWriteRequest {
            subject: SubjectRequest {
                kind: SubjectKind::Issue,
                subject: self.issue,
            },
            target: CommentTargetKind::Issue,
            entity: None,
            comment: None,
            body: Some(self.body),
            reply_to: self.reply_to,
            location: None,
            embeds: self.embeds,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct SecurityIssueRequest {
    pub(super) title: String,
    pub(super) description: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SecurityIssueResult {
    pub(super) repository: String,
    pub(super) issue: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct EditIssueRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct SetStateRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) state: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct SetStringsRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) values: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct CreatePatchRequest {
    pub(super) title: String,
    pub(super) description: String,
    pub(super) base: String,
    pub(super) head: String,
    #[serde(default)]
    pub(super) labels: Vec<String>,
    #[serde(default)]
    pub(super) draft: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct UpdatePatchRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) description: String,
    pub(super) base: String,
    pub(super) head: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct EditPatchRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) title: Option<String>,
    pub(super) revision: Option<String>,
    pub(super) description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct ReviewWriteRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) revision: Option<String>,
    pub(super) review: Option<String>,
    pub(super) verdict: Option<String>,
    pub(super) summary: Option<String>,
    #[serde(default)]
    pub(super) labels: Vec<String>,
    #[serde(default)]
    pub(super) embeds: Vec<EmbedWriteRequest>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct MergePatchRequest {
    #[serde(flatten)]
    pub(super) subject: SubjectRequest,
    pub(super) revision: String,
    pub(super) commit: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DiffDocument {
    pub(super) base: String,
    pub(super) head: String,
    pub(super) files: Vec<DiffFile>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DiffFile {
    pub(super) old_path: Option<String>,
    pub(super) new_path: Option<String>,
    pub(super) lines: Vec<DiffLine>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DiffLine {
    pub(super) origin: char,
    pub(super) old_line: Option<u32>,
    pub(super) new_line: Option<u32>,
    pub(super) content: String,
}

pub(super) fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(super) fn mutation_identity(
    method: &str,
    request: &impl Serialize,
) -> Result<String, ServiceError> {
    let payload = serde_json::to_vec(&(method, request))
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    git2::Oid::hash_object(git2::ObjectType::Blob, &payload)
        .map(|id| id.to_string())
        .map_err(|error| ServiceError::Request(stringify(error)))
}

pub(super) fn write_result(value: impl ToString) -> Result<Json<Document>, ServiceError> {
    Ok(Json(Document {
        data: serde_json::json!({ "id": value.to_string() }),
    }))
}

pub(super) fn require_kind(actual: SubjectKind, expected: SubjectKind) -> Result<(), ServiceError> {
    (actual == expected)
        .then_some(())
        .ok_or_else(|| ServiceError::Request("subject kind does not match method".to_owned()))
}

pub(super) fn parse_object_string(value: &str) -> Result<ObjectId, ServiceError> {
    parse_object(value).map_err(|error| ServiceError::Request(error.to_string()))
}

pub(super) fn parse_subject_request(
    subject: &SubjectRequest,
    expected: SubjectKind,
) -> Result<ObjectId, ServiceError> {
    require_kind(subject.kind, expected)?;
    parse_object_string(&subject.subject)
}

pub(super) fn parse_issue_state(value: &str) -> Result<issue::State, ServiceError> {
    match value {
        "open" => Ok(issue::State::Open),
        "closed" | "closed_other" => Ok(issue::State::Closed {
            reason: issue::CloseReason::Other,
        }),
        "solved" | "closed_solved" => Ok(issue::State::Closed {
            reason: issue::CloseReason::Solved,
        }),
        _ => Err(ServiceError::Request(
            "state must be open, closed, or solved".to_owned(),
        )),
    }
}

pub(super) fn parse_patch_state(value: &str) -> Result<patch::Lifecycle, ServiceError> {
    match value {
        "open" => Ok(patch::Lifecycle::Open),
        "draft" => Ok(patch::Lifecycle::Draft),
        "archived" => Ok(patch::Lifecycle::Archived),
        _ => Err(ServiceError::Request(
            "state must be open, draft, or archived".to_owned(),
        )),
    }
}

pub(super) fn parse_patch_commits(
    description: String,
    base: String,
    head: String,
    workspace: std::path::PathBuf,
) -> Result<PatchCommits, ServiceError> {
    Ok(PatchCommits {
        description,
        base: radicle::git::Oid::from_str(&base)
            .map_err(|error| ServiceError::Request(stringify(error)))?,
        head: radicle::git::Oid::from_str(&head)
            .map_err(|error| ServiceError::Request(stringify(error)))?,
        workspace,
    })
}

pub(super) fn parse_entry_string(value: &str) -> Result<EntryId, ServiceError> {
    parse_entry(value).map_err(|error| ServiceError::Request(error.to_string()))
}

pub(super) fn parse_revision(value: EntryId) -> Result<patch::RevisionId, ServiceError> {
    Ok(patch::RevisionId::from(value))
}

pub(super) fn parse_review(value: EntryId) -> Result<patch::ReviewId, ServiceError> {
    Ok(patch::ReviewId::from(value))
}

pub(super) fn parse_labels(values: Vec<String>) -> Result<Vec<radicle::cob::Label>, ServiceError> {
    values
        .into_iter()
        .map(|value| {
            radicle::cob::Label::from_str(&value)
                .map_err(|error| ServiceError::Request(stringify(error)))
        })
        .collect::<Result<BTreeSet<_>, _>>()
        .map(BTreeSet::into_iter)
        .map(Iterator::collect)
}

pub(super) fn parse_dids(values: Vec<String>) -> Result<Vec<radicle::identity::Did>, ServiceError> {
    values
        .into_iter()
        .map(|value| {
            radicle::identity::Did::from_str(&value)
                .map_err(|error| ServiceError::Request(stringify(error)))
        })
        .collect::<Result<BTreeSet<_>, _>>()
        .map(BTreeSet::into_iter)
        .map(Iterator::collect)
}

pub(super) fn parse_verdict(value: Option<&str>) -> Result<Option<patch::Verdict>, ServiceError> {
    match value {
        None | Some("none") => Ok(None),
        Some("accept") => Ok(Some(patch::Verdict::Accept)),
        Some("reject") => Ok(Some(patch::Verdict::Reject)),
        Some(_) => Err(ServiceError::Request(
            "verdict must be accept, reject, or none".to_owned(),
        )),
    }
}

pub(super) fn parse_location(
    request: LocationRequest,
) -> Result<radicle::cob::CodeLocation, ServiceError> {
    fn range(
        start: Option<usize>,
        end: Option<usize>,
    ) -> Result<Option<radicle::cob::CodeRange>, ServiceError> {
        match (start, end) {
            (None, None) => Ok(None),
            (Some(start), Some(end)) if start < end => {
                Ok(Some(radicle::cob::CodeRange::Lines { range: start..end }))
            }
            _ => Err(ServiceError::Request(
                "line ranges require start < end".to_owned(),
            )),
        }
    }
    Ok(radicle::cob::CodeLocation {
        commit: radicle::git::Oid::from_str(&request.commit)
            .map_err(|error| ServiceError::Request(stringify(error)))?,
        path: request.path.into(),
        old: range(request.old_start, request.old_end)?,
        new: range(request.new_start, request.new_end)?,
    })
}

pub(super) fn import_commits(
    repository: &radicle::storage::git::Repository,
    workspace: &std::path::Path,
    namespace: radicle::crypto::PublicKey,
    commits: &[radicle::git::Oid],
) -> Result<(), ServiceError> {
    let bare = git2::Repository::open_bare(repository.path())
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    let source = workspace.to_string_lossy();
    let mut remote = bare
        .remote_anonymous(&source)
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    for commit in commits {
        let reference = format!("refs/namespaces/{namespace}/refs/radicle-team/commits/{commit}");
        let refspec = format!("+{commit}:{reference}");
        remote
            .fetch(&[&refspec], None, None)
            .map_err(|error| ServiceError::Request(stringify(error)))?;
    }
    Ok(())
}

pub(super) fn parse_object(value: &str) -> Result<ObjectId, ServiceError> {
    ObjectId::from_str(value).map_err(|_error| ServiceError::InvalidId(value.to_owned()))
}

pub(super) fn parse_entry(value: &str) -> Result<EntryId, ServiceError> {
    EntryId::from_str(value).map_err(|_error| ServiceError::InvalidId(value.to_owned()))
}

pub(super) fn validate_merge(
    repository: &radicle::storage::git::Repository,
    patch: &patch::Patch,
    signer: &radicle::node::device::BoxedDevice,
    revision: patch::RevisionId,
    commit: radicle::git::Oid,
) -> Result<(), ServiceError> {
    if patch.revision(&revision).is_none() {
        return Err(ServiceError::Request(
            "patch revision is unavailable".to_owned(),
        ));
    }
    let branch = repository
        .default_branch()
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    let head = repository
        .reference_oid(signer.public_key(), &branch)
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    if commit != head
        && !repository
            .is_ancestor_of(commit, head)
            .map_err(|error| ServiceError::Request(stringify(error)))?
    {
        return Err(ServiceError::Request(
            "merge commit is not reachable from the delegate's default branch".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_code_location(
    repository: &radicle::storage::git::Repository,
    revision: &patch::Revision,
    location: &radicle::cob::CodeLocation,
) -> Result<(), ServiceError> {
    if location.commit != revision.head() {
        return Err(ServiceError::Request(
            "location commit must be the reviewed revision head".to_owned(),
        ));
    }
    if location.old.is_none() && location.new.is_none() {
        return Err(ServiceError::Request(
            "location must include an old or new line range".to_owned(),
        ));
    }
    let base = repository
        .backend
        .find_commit((*revision.base()).into())
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    let head = repository
        .backend
        .find_commit(revision.head().into())
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    let diff = repository
        .backend
        .diff_tree_to_tree(
            Some(
                &base
                    .tree()
                    .map_err(|error| ServiceError::Request(stringify(error)))?,
            ),
            Some(
                &head
                    .tree()
                    .map_err(|error| ServiceError::Request(stringify(error)))?,
            ),
            None,
        )
        .map_err(|error| ServiceError::Request(stringify(error)))?;
    let mut matched = false;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path_matches = delta.old_file().path() == Some(location.path.as_path())
            || delta.new_file().path() == Some(location.path.as_path());
        if !path_matches {
            return true;
        }
        matched |= range_contains(location.old.as_ref(), line.old_lineno())
            || range_contains(location.new.as_ref(), line.new_lineno());
        true
    })
    .map_err(|error| ServiceError::Request(stringify(error)))?;
    matched.then_some(()).ok_or_else(|| {
        ServiceError::Request("location does not intersect the reviewed diff".to_owned())
    })
}

fn range_contains(range: Option<&radicle::cob::CodeRange>, line: Option<u32>) -> bool {
    match (range, line) {
        (Some(radicle::cob::CodeRange::Lines { range }), Some(line)) => {
            usize::try_from(line).is_ok_and(|line| range.contains(&line))
        }
        (Some(radicle::cob::CodeRange::Chars { line: expected, .. }), Some(line)) => {
            usize::try_from(line).is_ok_and(|line| line == *expected)
        }
        _ => false,
    }
}
