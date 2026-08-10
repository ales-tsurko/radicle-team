//! Repository manifest for a Radicle team.
//!
//! [`Manifest`] loads the single `radicle-team.toml` manifest from a repository's canonical commit
//! or from one explicitly selected administrator-owned file. The file is shared repository policy,
//! not machine-local application state: contributors must agree on its reviewed character-to-DID
//! bindings, while private identity material remains in the application home. `config-rs` parses
//! the versioned document. Domain validation then rejects unknown members and duplicated active
//! DIDs, and verifies that the selected member is active and bound to the locally loaded
//! [`Identity`].
//!
//! Member identity bindings are one security-sensitive section of the manifest. Validating one
//! produces [`ValidatedBinding`], which carries the member, DID, generation, and source revision
//! used by later processing. The watcher must not run before that validation succeeds.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    str::{self, FromStr as _},
    sync::Arc,
};

use age::x25519;
use config::{Config as ConfigLoader, File, FileFormat};
use radicle::{
    git::{Oid, raw},
    identity::{Did, RepoId},
    storage::{ReadRepository as _, RepositoryError, git::Repository},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{
    chat::Config as ChatConfig,
    identity::{Identity, permissions::Permissions},
    member::{Member, UnknownMember},
};

/// Name of the repository manifest file.
///
/// Not hidden, because it is read and edited by whoever reviews the team's composition, and a file
/// nobody sees is a file nobody reviews.
pub const FILE_NAME: &str = "radicle-team.toml";
const SCHEMA_VERSION: u32 = 1;
const DEFAULT_FRONT_LABEL: &str = "urgent";
const DEFAULT_NEXT_LABEL: &str = "next";
const DEFAULT_EXCHANGES: usize = 3;

/// Parsed repository manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    members: BTreeMap<Member, Binding>,
    security: SecurityPolicy,
    triggered_by: TriggerAccess,
    priority: PriorityLabels,
    limits: Limits,
    chat: Option<Arc<ChatConfig>>,
    revision: Revision,
}

impl Manifest {
    /// Returns the security-reporting policy.
    #[must_use]
    pub const fn security(&self) -> &SecurityPolicy {
        &self.security
    }

    /// Returns the repository's IRC configuration, when chat is enabled.
    #[must_use]
    pub fn chat(&self) -> Option<&ChatConfig> {
        self.chat.as_deref()
    }

    /// Loads the manifest from the repository's canonical branch.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical commit or manifest cannot be read and validated.
    pub fn load_repository(repository: &Repository) -> Result<Self, Error> {
        let (_, commit) = repository.canonical_head()?;
        let blob = repository
            .blob_at(commit, FILE_NAME)
            .map_err(|source| Error::Git { commit, source })?;

        Self::parse(
            str::from_utf8(blob.content())?,
            Revision::Repository(commit),
        )
    }

    /// Loads the manifest when the repository contains it.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical commit or an existing manifest cannot be read and
    /// validated.
    pub fn load_optional_repository(repository: &Repository) -> Result<Option<Self>, Error> {
        match Self::load_repository(repository) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(Error::Git { source, .. }) if source.code() == raw::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Loads the manifest from an explicit administrator-owned file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file has unsafe permissions or cannot be read and validated.
    pub fn load_file(path: &Path) -> Result<Self, Error> {
        Permissions::administrator_file(path)?;
        let contents = fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Self::parse(&contents, Revision::External)
    }

    /// Loads an explicit administrator-owned manifest when it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing file has unsafe permissions or cannot be read and
    /// validated.
    pub fn load_optional_file(path: &Path) -> Result<Option<Self>, Error> {
        match fs::symlink_metadata(path) {
            Ok(_) => Self::load_file(path).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses proposed manifest contents and validates the selected binding.
    ///
    /// # Errors
    ///
    /// Returns an error when the contents or selected binding are invalid.
    pub(crate) fn validate_candidate(
        contents: &str,
        member: Member,
        did: Did,
    ) -> Result<ValidatedBinding, Error> {
        Self::parse(contents, Revision::External)?.validate_did(member, did)
    }

    /// Validates the selected member against the local signing identity.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, inactive, duplicated, or mismatched bindings.
    pub fn validate(&self, member: Member, identity: &Identity) -> Result<ValidatedBinding, Error> {
        self.validate_did(member, identity.did())
    }

    pub(crate) fn validate_member(
        &self,
        member: Member,
        did: Did,
    ) -> Result<ValidatedBinding, Error> {
        self.validate_did(member, did)
    }

    fn validate_did(&self, member: Member, did: Did) -> Result<ValidatedBinding, Error> {
        self.validate_unique_active_dids()?;
        let binding = self
            .members
            .get(&member)
            .ok_or(Error::Missing { member, did })?;
        if binding.state != BindingState::Active {
            return Err(Error::Inactive(member));
        }
        if binding.signing_did != did {
            return Err(Error::DidMismatch {
                member,
                binding: binding.signing_did,
                local: did,
            });
        }

        Ok(ValidatedBinding::new(
            member,
            binding.signing_did,
            binding.generation,
            self.revision,
            binding.mcp_allow.clone(),
            self.security.clone(),
            RepositoryPolicy::new(
                self.triggered_by.clone(),
                self.active_member_dids(),
                self.priority.clone(),
                self.limits.clone(),
            )
            .with_chat(self.chat.clone()),
        ))
    }

    fn active_member_dids(&self) -> Vec<Did> {
        self.members
            .values()
            .filter(|binding| binding.state == BindingState::Active)
            .map(|binding| binding.signing_did)
            .collect()
    }

    fn parse(contents: &str, revision: Revision) -> Result<Self, Error> {
        let document = ConfigLoader::builder()
            .add_source(File::from_str(contents, FileFormat::Toml))
            .build()?
            .try_deserialize::<Document>()?;
        if document.version != SCHEMA_VERSION {
            return Err(Error::Schema {
                actual: document.version,
            });
        }
        let members: BTreeMap<Member, Binding> = document
            .members
            .into_iter()
            .map(|(name, binding)| Ok((name.parse()?, binding)))
            .collect::<Result<_, Error>>()?;

        let active_dids = members
            .values()
            .filter(|binding| binding.state == BindingState::Active)
            .map(|binding| binding.signing_did)
            .collect::<Vec<_>>();
        let chat = document
            .chat
            .map(|chat| chat.validate(&active_dids).map_err(Error::Chat))
            .transpose()?
            .map(Arc::new);

        Ok(Self {
            members,
            security: document.security,
            triggered_by: document.triggered_by,
            priority: document.priority,
            limits: document.limits,
            chat,
            revision,
        })
    }

    fn validate_unique_active_dids(&self) -> Result<(), Error> {
        let mut active = BTreeMap::<Did, Member>::new();
        for (member, binding) in &self.members {
            if binding.state != BindingState::Active {
                continue;
            }
            if let Some(first) = active.insert(binding.signing_did, *member) {
                return Err(Error::DuplicateDid {
                    first,
                    second: *member,
                    did: binding.signing_did,
                });
            }
        }

        Ok(())
    }
}

/// One repository's binding from a fixed member to a signing DID.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    signing_did: Did,
    generation: u64,
    state: BindingState,
    mcp_allow: McpAllowlist,
}

impl Binding {
    /// Returns the configured signing DID.
    #[must_use]
    pub const fn signing_did(&self) -> Did {
        self.signing_did
    }

    /// Returns the binding generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the binding state.
    #[must_use]
    pub const fn state(&self) -> BindingState {
        self.state
    }

    /// Returns the MCP methods allowed for this binding.
    #[must_use]
    pub fn mcp_allow(&self) -> &McpAllowlist {
        &self.mcp_allow
    }
}

/// Whether a repository member binding may receive work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingState {
    /// The member may receive work.
    Active,
    /// The binding exists but may not receive or publish work.
    Inactive,
}

/// MCP methods allowed for one member in one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpAllowlist(BTreeSet<String>);

impl McpAllowlist {
    fn from_entries(entries: Vec<String>) -> Result<Self, UnknownMcpPermission> {
        let mut allowlist = Self::default();
        for entry in entries {
            if Self::is_group(&entry) {
                allowlist.extend_group(&entry);
            } else if !Self::is_unrestricted(&entry)
                && crate::mcp::ToolService::registered_tools()
                    .iter()
                    .any(|method| method == &entry)
            {
                allowlist.insert(&entry);
            } else {
                return Err(UnknownMcpPermission(entry));
            }
        }
        Ok(allowlist)
    }

    /// Returns whether the manifest has no say in `method`.
    fn is_unrestricted(method: &str) -> bool {
        crate::mcp::ToolService::is_unrestricted(method)
    }

    /// Allows every method the manifest is able to grant.
    #[cfg(all(test, not(miri)))]
    pub(crate) fn everything() -> Self {
        let mut allowlist = Self::default();
        for method in crate::mcp::ToolService::registered_tools() {
            if !Self::is_unrestricted(method) {
                allowlist.insert(method);
            }
        }
        allowlist
    }

    fn extend_group(&mut self, group: &str) {
        for method in crate::mcp::ToolService::registered_tools() {
            if Self::belongs_to_group(method, group) {
                self.insert(method);
            }
        }
    }

    fn insert(&mut self, method: &str) {
        let _inserted = self.0.insert(method.to_owned());
    }

    /// Returns whether the named MCP method is allowed.
    #[must_use]
    pub fn allows(&self, method: &str) -> bool {
        self.0.contains(method) || Self::is_unrestricted(method)
    }

    /// Returns the allowed MCP method names.
    pub fn methods(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    fn is_group(value: &str) -> bool {
        matches!(
            value,
            "read" | "chat" | "comments" | "issues" | "patches" | "reactions" | "reviews"
        )
    }

    fn belongs_to_group(method: &str, group: &str) -> bool {
        if Self::is_unrestricted(method) {
            return false;
        }
        let read = method.starts_with("get_") || method.starts_with("list_");
        let names = |entity| method.split('_').any(|part| part == entity);

        match group {
            "read" => read,
            "chat" => method == "reply_chat",
            "comments" => !read && method != "comment_issue" && names("comment"),
            "issues" => !read && names("issue"),
            "patches" => !read && method != "merge_patch" && names("patch"),
            "reactions" => !read && names("reaction"),
            "reviews" => !read && names("review"),
            _ => false,
        }
    }
}

impl<'de> Deserialize<'de> for McpAllowlist {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_entries(Vec::<String>::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Identities allowed to trigger this repository's agents.
///
/// Repository delegates are always allowed, because they control the manifest and could grant
/// themselves access by editing it. Restricting them would express an intent the manifest cannot
/// enforce.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TriggerAccess {
    /// Only repository delegates.
    #[default]
    Delegates,
    /// Any identity that can author an operation in this repository.
    Anyone,
    /// Repository delegates and the listed identities.
    Dids(BTreeSet<Did>),
}

impl TriggerAccess {
    /// Returns whether `author` may trigger agent work, given the repository's `delegates`.
    #[must_use]
    pub fn allows(&self, author: Did, delegates: &[Did]) -> bool {
        match self {
            Self::Anyone => true,
            Self::Delegates => delegates.contains(&author),
            Self::Dids(dids) => dids.contains(&author) || delegates.contains(&author),
        }
    }
}

impl Serialize for TriggerAccess {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Delegates => serializer.serialize_str("delegates"),
            Self::Anyone => serializer.serialize_str("anyone"),
            Self::Dids(dids) => serializer.collect_seq(dids.iter().map(Did::to_string)),
        }
    }
}

impl<'de> Deserialize<'de> for TriggerAccess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match TriggerValue::deserialize(deserializer)? {
            TriggerValue::Mode(TriggerMode::Delegates) => Ok(Self::Delegates),
            TriggerValue::Mode(TriggerMode::Anyone) => Ok(Self::Anyone),
            TriggerValue::Dids(dids) => {
                if dids.is_empty() {
                    return Err(de::Error::custom(
                        "'triggered_by' cannot be an empty list; use \"delegates\" or \"anyone\"",
                    ));
                }
                dids.into_iter()
                    .map(|did| Did::from_str(&did).map_err(de::Error::custom))
                    .collect::<Result<_, _>>()
                    .map(Self::Dids)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TriggerValue {
    Mode(TriggerMode),
    Dids(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum TriggerMode {
    Delegates,
    Anyone,
}

/// Revision from which the manifest was loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revision {
    /// Canonical repository commit containing the manifest.
    Repository(Oid),
    /// Explicit administrator-owned file.
    External,
}

/// Identities allowed to put this repository's members to work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryPolicy {
    access: TriggerAccess,
    members: Vec<Did>,
    priority: PriorityLabels,
    limits: Limits,
    chat: Option<Arc<ChatConfig>>,
}

impl RepositoryPolicy {
    /// Pairs an access rule with every active member's signing DID.
    #[must_use]
    pub const fn new(
        access: TriggerAccess,
        members: Vec<Did>,
        priority: PriorityLabels,
        limits: Limits,
    ) -> Self {
        Self {
            access,
            members,
            priority,
            limits,
            chat: None,
        }
    }

    fn with_chat(mut self, chat: Option<Arc<ChatConfig>>) -> Self {
        self.chat = chat;
        self
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn with_chat_for_test(mut self, chat: ChatConfig) -> Self {
        self.chat = Some(Arc::new(chat));
        self
    }

    /// Returns the bounds on conversation between this repository's members.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Returns the labels which move assigned work ahead of the queue.
    #[must_use]
    pub const fn priority(&self) -> &PriorityLabels {
        &self.priority
    }

    /// Returns the configured access rule.
    #[must_use]
    pub const fn access(&self) -> &TriggerAccess {
        &self.access
    }

    /// Returns every active member's signing DID.
    #[must_use]
    pub fn members(&self) -> &[Did] {
        &self.members
    }

    /// Returns the repository's validated IRC configuration, when chat is enabled.
    #[must_use]
    pub fn chat(&self) -> Option<&ChatConfig> {
        self.chat.as_deref()
    }
}

/// An active binding verified against the local signing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedBinding {
    member: Member,
    did: Did,
    generation: u64,
    revision: Revision,
    mcp_allow: McpAllowlist,
    security: SecurityPolicy,
    triggers: RepositoryPolicy,
}

impl ValidatedBinding {
    pub(super) const fn new(
        member: Member,
        did: Did,
        generation: u64,
        revision: Revision,
        mcp_allow: McpAllowlist,
        security: SecurityPolicy,
        triggers: RepositoryPolicy,
    ) -> Self {
        Self {
            member,
            did,
            generation,
            revision,
            mcp_allow,
            security,
            triggers,
        }
    }

    /// Returns the repository-wide policy this binding was validated against.
    #[must_use]
    pub const fn policy(&self) -> &RepositoryPolicy {
        &self.triggers
    }

    /// Returns the bound member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.member
    }

    /// Returns the verified signing DID.
    #[must_use]
    pub const fn did(&self) -> Did {
        self.did
    }

    /// Returns the verified binding generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the source revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the MCP methods allowed for this binding.
    #[must_use]
    pub fn mcp_allow(&self) -> &McpAllowlist {
        &self.mcp_allow
    }

    /// Returns the security-reporting policy.
    #[must_use]
    pub const fn security(&self) -> &SecurityPolicy {
        &self.security
    }

    /// Returns the validated IRC configuration, when chat is enabled.
    #[must_use]
    pub fn chat(&self) -> Option<&ChatConfig> {
        self.triggers.chat()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    security: SecurityPolicy,
    #[serde(default)]
    triggered_by: TriggerAccess,
    #[serde(default)]
    priority: PriorityLabels,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    chat: Option<ChatConfig>,
    members: BTreeMap<String, Binding>,
}

/// Bounds on how far this repository's members may carry a conversation by themselves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    exchanges: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exchanges: DEFAULT_EXCHANGES,
        }
    }
}

impl Limits {
    /// Returns how many turns one member may take with another on the same issue or patch before
    /// a person has to join the conversation.
    #[must_use]
    pub const fn exchanges(&self) -> usize {
        self.exchanges
    }
}

/// Issue labels which move assigned work ahead of the queue.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriorityLabels {
    front: String,
    next: String,
}

impl Default for PriorityLabels {
    fn default() -> Self {
        Self {
            front: DEFAULT_FRONT_LABEL.to_owned(),
            next: DEFAULT_NEXT_LABEL.to_owned(),
        }
    }
}

impl PriorityLabels {
    /// Returns the queue position `labels` ask for.
    #[must_use]
    pub fn position<'a>(&self, labels: impl IntoIterator<Item = &'a str>) -> QueuePosition {
        let mut position = QueuePosition::End;
        for label in labels {
            if label == self.front {
                return QueuePosition::Front;
            }
            if label == self.next {
                position = QueuePosition::Next;
            }
        }
        position
    }
}

/// Where new work joins the member's queue.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum QueuePosition {
    /// Ahead of everything already waiting.
    Front,
    /// After the running job, ahead of everything else waiting.
    Next,
    /// Behind everything already waiting.
    #[default]
    End,
}

/// Private repository and encryption recipients for security reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityPolicy {
    repository: RepoId,
    recipients: Vec<String>,
}

impl SecurityPolicy {
    /// Returns the private repository receiving encrypted security reports.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.repository
    }

    /// Returns the age recipients allowed to decrypt security reports.
    pub fn recipients(&self) -> &[String] {
        &self.recipients
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn test(repository: RepoId) -> Self {
        Self::test_with_recipients(
            repository,
            vec!["age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa".to_owned()],
        )
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn test_with_recipients(repository: RepoId, recipients: Vec<String>) -> Self {
        Self {
            repository,
            recipients,
        }
    }
}

impl<'de> Deserialize<'de> for SecurityPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Policy {
            repository: RepoId,
            recipients: Vec<String>,
        }

        let policy = Policy::deserialize(deserializer)?;
        if policy.recipients.is_empty() {
            return Err(de::Error::custom(
                "security reporting must contain at least one age recipient",
            ));
        }
        let mut seen = BTreeSet::new();
        let recipients = policy
            .recipients
            .into_iter()
            .map(|value| {
                x25519::Recipient::from_str(&value).map_err(de::Error::custom)?;
                if !seen.insert(value.clone()) {
                    return Err(de::Error::custom(format!(
                        "duplicate security-reporting recipient '{value}'"
                    )));
                }
                Ok(value)
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            repository: policy.repository,
            recipients,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown MCP permission '{0}'")]
struct UnknownMcpPermission(String);

/// Error loading or validating a repository manifest.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading an external manifest failed.
    #[error("failed to read repository manifest from '{path}': {source}")]
    Io {
        /// Manifest path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The external manifest has unsafe ownership or permissions.
    #[error("unsafe repository manifest: {0}")]
    Permissions(#[from] crate::identity::PermissionError),
    /// Computing the canonical repository head failed.
    #[error("failed to resolve canonical repository head: {0}")]
    Repository(#[from] RepositoryError),
    /// Reading the manifest from Git failed.
    #[error("failed to read '{FILE_NAME}' from canonical commit {commit}: {source}")]
    Git {
        /// Canonical commit.
        commit: Oid,
        /// Git error.
        source: raw::Error,
    },
    /// The manifest is not UTF-8.
    #[error("repository manifest is not valid UTF-8: {0}")]
    Utf8(#[from] str::Utf8Error),
    /// Manifest parsing failed.
    #[error("invalid repository manifest: {0}")]
    Parse(#[from] config::ConfigError),
    /// The manifest schema is unsupported.
    #[error("unsupported repository manifest schema {actual}; expected {SCHEMA_VERSION}")]
    Schema {
        /// Unsupported schema version.
        actual: u32,
    },
    /// The manifest names a character that is not built in.
    #[error(transparent)]
    UnknownMember(#[from] UnknownMember),
    /// Two active members use the same signing identity.
    #[error("members {first} and {second} both use active signing DID {did}")]
    DuplicateDid {
        /// First member using the DID.
        first: Member,
        /// Second member using the DID.
        second: Member,
        /// Duplicated DID.
        did: Did,
    },
    /// The selected member is absent.
    #[error("repository manifest has no binding for member {member}; add {did} to '{FILE_NAME}'")]
    Missing {
        /// Selected member.
        member: Member,
        /// Local identity to bind.
        did: Did,
    },
    /// The selected member is inactive.
    #[error("repository manifest binding for member {0} is inactive")]
    Inactive(Member),
    /// The binding and local signing identity differ.
    #[error("repository manifest binds member {member} to {binding}, but the key derives {local}")]
    DidMismatch {
        /// Selected member.
        member: Member,
        /// Repository binding.
        binding: Did,
        /// Local identity.
        local: Did,
    },
    /// Chat configuration or an identity attestation is invalid.
    #[error("invalid chat configuration: {0}")]
    Chat(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use radicle::identity::Did;
    #[cfg(not(miri))]
    use radicle::{git::Oid, identity::RepoId};

    use crate::{
        manifest::{Error, Manifest, McpAllowlist, QueuePosition, Revision, TriggerAccess},
        member::Member,
    };

    const NICO: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
    const MAYA: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";
    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const RECIPIENT: &str = "age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa";

    struct Fixture;

    impl Fixture {
        fn parse(body: &str) -> Result<Manifest, Error> {
            let policy = format!(
                "\n[security]\nrepository = \"{REPOSITORY}\"\nrecipients = [\"{RECIPIENT}\"]\n"
            );
            let body = body.replacen('\n', &policy, 1);
            Manifest::parse(&body, Revision::External)
        }
    }

    #[test]
    fn accepts_a_missing_explicit_manifest_during_setup() {
        let path = PathBuf::from("target/tests/manifest/missing.toml");
        let _result = fs::remove_file(&path);

        assert!(matches!(Manifest::load_optional_file(&path), Ok(None)));
    }

    #[test]
    fn parses_repository_manifest() {
        let manifest = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = [\"read\"]\n"
        ))
        .expect("manifest should parse");
        let binding = manifest
            .members
            .get(&Member::Nico)
            .expect("Nico should exist");

        assert_eq!(
            binding.signing_did(),
            NICO.parse::<Did>().expect("DID should parse")
        );
        assert_eq!(binding.generation(), 1);
        assert!(binding.mcp_allow().allows("get_subject"));
        assert!(binding.mcp_allow().allows("get_diff"));
        assert!(!binding.mcp_allow().allows("create_comment"));
    }

    #[cfg(not(miri))]
    #[test]
    fn validates_chat_configuration_and_authenticates_only_the_pinned_account() {
        use radicle::crypto::{Signature, signature::Signer as _, ssh::keystore::MemorySigner};

        use crate::chat::{
            AttestationPayload, ClientCertificateFingerprint, IrcAccount, Server, ServerEndpoint,
            ServerPin,
        };

        const SERVER_PIN: &str =
            "spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const OTHER_SERVER_PIN: &str =
            "spki-sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const CERTIFICATE: &str =
            "cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        let signer = MemorySigner::r#gen();
        let did = Did::from(*radicle::crypto::Signer::public_key(&signer));
        let repository = REPOSITORY.parse().expect("RID should parse");
        let server = Server::new(
            "ircs://irc.example.com:6697"
                .parse::<ServerEndpoint>()
                .expect("endpoint should parse"),
            SERVER_PIN.parse().expect("server pin should parse"),
        );
        let account = "nico".parse::<IrcAccount>().expect("account should parse");
        let certificate = CERTIFICATE
            .parse::<ClientCertificateFingerprint>()
            .expect("certificate should parse");
        let payload = AttestationPayload::new(repository, server, account, certificate, did);
        let signature: Signature = signer.sign(payload.canonical().as_bytes());
        let body = format!(
            "version = 1\n[chat]\nrepository = \"{REPOSITORY}\"\n[chat.server]\nendpoint = \
             \"ircs://irc.example.com:6697\"\npin = \"{SERVER_PIN}\"\n[chat.channels]\nteam = \
             \"#project\"\n[[chat.participants]]\naccount = \"nico\"\ncertificate = \
             \"{CERTIFICATE}\"\ndid = \"{did}\"\nsignature = \
             \"{signature}\"\n[members.nico]\nsigning_did = \"{did}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = [\"chat\"]\n"
        );
        let manifest = Fixture::parse(&body).expect("signed chat manifest should parse");
        let chat = manifest.chat().expect("chat should be configured");
        let nico = "nico".parse::<IrcAccount>().expect("account should parse");
        let maya = "maya".parse::<IrcAccount>().expect("account should parse");

        let sender = chat
            .authenticate(repository, chat.server().pin(), &nico)
            .expect("attested account should authenticate");
        assert_eq!(sender.did(), did);
        assert!(
            chat.authenticate(
                repository,
                OTHER_SERVER_PIN
                    .parse::<ServerPin>()
                    .expect("other server pin should parse"),
                &nico,
            )
            .is_err(),
            "another server must not authenticate the same account"
        );
        assert!(
            chat.authenticate(repository, chat.server().pin(), &maya)
                .is_err(),
            "an unattested account must fail closed"
        );
        let another_repository = RepoId::from(Oid::from_sha1([1; 20]));
        assert!(
            chat.authenticate(another_repository, chat.server().pin(), &nico)
                .is_err(),
            "an attestation must not cross repositories"
        );

        let tampered = body.replace("account = \"nico\"", "account = \"maya\"");
        assert!(matches!(Fixture::parse(&tampered), Err(Error::Chat(_))));
    }

    #[test]
    fn rejects_a_missing_mcp_allowlist() {
        assert!(matches!(
            Fixture::parse(&format!(
                "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
                 \"active\"\n"
            )),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn rejects_a_missing_security_policy() {
        assert!(matches!(
            Manifest::parse("version = 1\n[members]\n", Revision::External),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn rejects_empty_duplicate_and_invalid_security_recipients() {
        for recipients in [
            "[]".to_owned(),
            format!("[\"{RECIPIENT}\", \"{RECIPIENT}\"]"),
            "[\"security@example.com\"]".to_owned(),
        ] {
            assert!(matches!(
                Manifest::parse(
                    &format!(
                        "version = 1\n[security]\nrepository = \"{REPOSITORY}\"\nrecipients = \
                         {recipients}\n[members]\n"
                    ),
                    Revision::External,
                ),
                Err(Error::Parse(_))
            ));
        }
    }

    #[test]
    fn expands_explicit_mcp_groups_without_implicitly_allowing_merge() {
        for group in [
            "read",
            "chat",
            "comments",
            "issues",
            "patches",
            "reactions",
            "reviews",
        ] {
            let allowlist =
                McpAllowlist::from_entries(vec![group.to_owned()]).expect("group should be known");
            assert_eq!(
                allowlist.methods().collect::<Vec<_>>(),
                crate::mcp::ToolService::registered_tools()
                    .iter()
                    .map(String::as_str)
                    .filter(|method| McpAllowlist::belongs_to_group(method, group))
                    .collect::<Vec<_>>()
            );
            assert!(!allowlist.allows("merge_patch"));
        }
    }

    #[test]
    fn accepts_explicit_methods_and_an_empty_allowlist() {
        let explicit = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = [\"merge_patch\"]\n"
        ))
        .expect("manifest should parse");
        let explicit = explicit
            .members
            .get(&Member::Nico)
            .expect("Nico should exist")
            .mcp_allow();
        assert_eq!(explicit.methods().collect::<Vec<_>>(), ["merge_patch"]);

        let empty = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        ))
        .expect("manifest should parse");
        assert_eq!(
            empty
                .members
                .get(&Member::Nico)
                .expect("Nico should exist")
                .mcp_allow()
                .methods()
                .count(),
            0
        );
    }

    #[test]
    fn accepts_every_registered_tool_without_a_manifest_method_registry() {
        for method in crate::mcp::ToolService::registered_tools() {
            if McpAllowlist::is_unrestricted(method) {
                McpAllowlist::from_entries(vec![method.clone()]).expect_err(
                    "a tool the manifest has no say in must not be usable as a permission",
                );
                continue;
            }
            let allowlist = McpAllowlist::from_entries(vec![method.clone()])
                .expect("registered tool should be accepted");
            assert!(allowlist.allows(method));
            assert!(
                method == "merge_patch"
                    || [
                        "read",
                        "chat",
                        "comments",
                        "issues",
                        "patches",
                        "reactions",
                        "reviews",
                    ]
                    .into_iter()
                    .any(|group| McpAllowlist::belongs_to_group(method, group)),
                "registered tool '{method}' must follow a permission-group naming convention"
            );
        }
    }

    #[test]
    fn rejects_unknown_mcp_permissions() {
        assert!(matches!(
            Fixture::parse(&format!(
                "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
                 \"active\"\nmcp_allow = [\"everything\"]\n"
            )),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn rejects_unknown_members() {
        assert!(matches!(
            Fixture::parse(&format!(
                "version = 1\n[members.alice]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
                 \"active\"\nmcp_allow = []\n"
            )),
            Err(Error::UnknownMember(_))
        ));
    }

    #[test]
    fn parses_every_trigger_access_form() {
        let member = format!(
            "[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        );
        let document = |triggered_by: &str| {
            format!(
                "version = 1\n{triggered_by}[security]\nrepository = \"{REPOSITORY}\"\nrecipients \
                 = [\"{RECIPIENT}\"]\n{member}"
            )
        };
        let parse = |value: &str| {
            Manifest::parse(
                &document(&format!("triggered_by = {value}\n")),
                Revision::External,
            )
            .map(|manifest| manifest.triggered_by)
        };

        assert_eq!(
            Manifest::parse(&document(""), Revision::External)
                .expect("a manifest without 'triggered_by' should parse")
                .triggered_by,
            TriggerAccess::Delegates,
            "omitting 'triggered_by' must fall back to delegates rather than opening the \
             repository"
        );
        assert_eq!(
            parse("\"delegates\"").expect("\"delegates\" should parse"),
            TriggerAccess::Delegates
        );
        assert_eq!(
            parse("\"anyone\"").expect("\"anyone\" should parse"),
            TriggerAccess::Anyone
        );
        assert_eq!(
            parse(&format!("[\"{NICO}\"]")).expect("a DID list should parse"),
            TriggerAccess::Dids(BTreeSet::from([NICO.parse().expect("DID should parse")]))
        );
        assert!(
            matches!(parse("[]"), Err(Error::Parse(_))),
            "an empty list must be rejected rather than silently denying everyone"
        );
        assert!(
            matches!(parse("\"everyone\""), Err(Error::Parse(_))),
            "an unknown mode must be rejected"
        );
        assert!(
            matches!(parse("[\"not-a-did\"]"), Err(Error::Parse(_))),
            "an invalid DID must be rejected"
        );
    }

    #[test]
    fn renames_the_priority_labels_without_touching_the_defaults() {
        let member = format!(
            "[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        );

        let default = Fixture::parse(&format!("version = 1\n{member}"))
            .expect("a manifest without '[priority]' should parse")
            .priority;
        assert_eq!(
            default.position(["chore", "urgent"]),
            QueuePosition::Front,
            "the defaults must work without the section, so an ordinary manifest says nothing \
             about scheduling"
        );
        assert_eq!(default.position(["chore", "next"]), QueuePosition::Next);
        assert_eq!(default.position(["chore"]), QueuePosition::End);

        let renamed = Fixture::parse(&format!(
            "version = 1\n[priority]\nfront = \"now\"\n{member}"
        ))
        .expect("a renamed label should parse");
        assert_eq!(renamed.priority.position(["now"]), QueuePosition::Front);
        assert_eq!(
            renamed.priority.position(["urgent"]),
            QueuePosition::End,
            "renaming one label must not leave the word it replaced still working"
        );
        assert_eq!(
            renamed.priority.position(["next"]),
            QueuePosition::Next,
            "the label that was not renamed keeps its default"
        );
    }

    #[test]
    fn overrides_the_exchange_limit_only_when_the_manifest_says_so() {
        let member = format!(
            "[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        );

        assert_eq!(
            Fixture::parse(&format!("version = 1\n{member}"))
                .expect("a manifest without '[limits]' should parse")
                .limits
                .exchanges(),
            3,
            "the default must work without the section, so an ordinary manifest says nothing \
             about how long its members may talk"
        );
        assert_eq!(
            Fixture::parse(&format!("version = 1\n[limits]\nexchanges = 8\n{member}"))
                .expect("an overridden limit should parse")
                .limits
                .exchanges(),
            8
        );
        assert!(
            matches!(
                Fixture::parse(&format!("version = 1\n[limits]\nrounds = 8\n{member}")),
                Err(Error::Parse(_))
            ),
            "an unknown key must be refused rather than silently leaving the default in place"
        );
    }

    #[test]
    fn rejects_unsupported_schema() {
        assert!(matches!(
            Fixture::parse("version = 2\n[members]\n"),
            Err(Error::Schema { actual: 2 })
        ));
    }

    #[test]
    fn rejects_unknown_project_settings() {
        assert!(matches!(
            Fixture::parse("version = 1\nunknown = true\n[members]\n"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn detects_duplicate_active_dids() {
        let bindings = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n[members.maya]\nsigning_did = \"{NICO}\"\ngeneration = \
             1\nstate = \"active\"\nmcp_allow = []\n"
        ))
        .expect("bindings should parse");

        assert!(matches!(
            bindings.validate_unique_active_dids(),
            Err(Error::DuplicateDid {
                first: Member::Nico,
                second: Member::Maya,
                ..
            })
        ));
    }

    #[test]
    fn permits_distinct_active_dids() {
        let bindings = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n[members.maya]\nsigning_did = \"{MAYA}\"\ngeneration = \
             1\nstate = \"active\"\nmcp_allow = []\n"
        ))
        .expect("bindings should parse");

        bindings
            .validate_unique_active_dids()
            .expect("distinct DIDs should validate");
    }

    #[test]
    fn validates_the_selected_member() {
        let bindings = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 3\nstate = \
             \"active\"\nmcp_allow = []\n"
        ))
        .expect("bindings should parse");
        let binding = bindings
            .validate_did(Member::Nico, NICO.parse().expect("local DID should parse"))
            .expect("binding should validate");

        assert_eq!(binding.member(), Member::Nico);
        assert_eq!(binding.generation(), 3);
        assert_eq!(binding.revision(), Revision::External);
    }

    #[test]
    fn rejects_missing_inactive_and_mismatched_bindings() {
        let bindings = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"inactive\"\nmcp_allow = []\n"
        ))
        .expect("bindings should parse");
        let nico = NICO.parse().expect("Nico DID should parse");
        let maya = MAYA.parse().expect("Maya DID should parse");

        assert!(matches!(
            bindings.validate_did(Member::Maya, maya),
            Err(Error::Missing {
                member: Member::Maya,
                ..
            })
        ));
        assert!(matches!(
            bindings.validate_did(Member::Nico, nico),
            Err(Error::Inactive(Member::Nico))
        ));

        let active = Fixture::parse(&format!(
            "version = 1\n[members.nico]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        ))
        .expect("bindings should parse");
        assert!(matches!(
            active.validate_did(Member::Nico, maya),
            Err(Error::DidMismatch { .. })
        ));
    }
}
