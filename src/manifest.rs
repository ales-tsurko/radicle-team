//! Repository manifest for a Radicle team.
//!
//! [`Manifest`](crate::manifest::Manifest) loads the single `.radicle-team.toml` manifest from a
//! repository's canonical commit or from one explicitly selected administrator-owned file. The file
//! is shared repository policy, not machine-local application state: contributors must agree on its
//! reviewed character-to-DID bindings, while private identity material remains in the application
//! home. `config-rs` parses the versioned document; domain validation rejects unknown members and
//! duplicated active DIDs, then verifies that the selected member is active and bound to the
//! locally loaded [`Identity`](crate::identity::Identity).
//!
//! Member identity bindings are one security-sensitive section of the manifest. Validating one
//! produces [`ValidatedBinding`](crate::manifest::ValidatedBinding), which carries the member, DID,
//! generation, and source revision used by later processing. The watcher must not run before that
//! validation succeeds.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    str,
};

use config::{Config as ConfigLoader, File, FileFormat};
use radicle::{
    git::{Oid, raw},
    identity::Did,
    storage::{ReadRepository as _, RepositoryError, git::Repository},
};
use serde::{Deserialize, Deserializer, de};

use crate::{
    identity::{Identity, permissions::Permissions},
    member::{Member, UnknownMember},
};

/// Name of the repository manifest file.
pub const FILE_NAME: &str = ".radicle-team.toml";
const SCHEMA_VERSION: u32 = 1;

/// Parsed repository manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    members: BTreeMap<Member, Binding>,
    revision: Revision,
}

impl Manifest {
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
        )
        .with_mcp_allow(binding.mcp_allow.clone()))
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
        let members = document
            .members
            .into_iter()
            .map(|(name, binding)| Ok((name.parse()?, binding)))
            .collect::<Result<_, Error>>()?;

        Ok(Self { members, revision })
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
    #[serde(default = "McpAllowlist::legacy")]
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
    fn legacy() -> Self {
        Self::from_group("read")
    }

    fn from_entries(entries: Vec<String>) -> Result<Self, UnknownMcpPermission> {
        let mut allowlist = Self::default();
        for entry in entries {
            if Self::is_group(&entry) {
                allowlist.extend_group(&entry);
            } else if crate::mcp::ToolService::registered_tools()
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

    fn from_group(group: &str) -> Self {
        let mut allowlist = Self::default();
        allowlist.extend_group(group);
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
        self.0.contains(method)
    }

    /// Returns the allowed MCP method names.
    pub fn methods(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    fn is_group(value: &str) -> bool {
        matches!(
            value,
            "read" | "comments" | "issues" | "patches" | "reviews"
        )
    }

    fn belongs_to_group(method: &str, group: &str) -> bool {
        let read = method.starts_with("get_") || method.starts_with("list_");
        let names = |entity| method.split('_').any(|part| part == entity);

        match group {
            "read" => read,
            "comments" => !read && names("comment"),
            "issues" => !read && names("issue"),
            "patches" => !read && method != "merge_patch" && names("patch"),
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

/// Revision from which the manifest was loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revision {
    /// Canonical repository commit containing the manifest.
    Repository(Oid),
    /// Explicit administrator-owned file.
    External,
}

/// An active binding verified against the local signing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedBinding {
    member: Member,
    did: Did,
    generation: u64,
    revision: Revision,
    mcp_allow: McpAllowlist,
}

impl ValidatedBinding {
    pub(super) const fn new(member: Member, did: Did, generation: u64, revision: Revision) -> Self {
        Self {
            member,
            did,
            generation,
            revision,
            mcp_allow: McpAllowlist(BTreeSet::new()),
        }
    }

    fn with_mcp_allow(mut self, mcp_allow: McpAllowlist) -> Self {
        self.mcp_allow = mcp_allow;
        self
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    members: BTreeMap<String, Binding>,
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
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use radicle::identity::Did;

    use crate::{
        manifest::{Error, Manifest, McpAllowlist, Revision},
        member::Member,
    };

    const NICO: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
    const MAYA: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";

    struct Fixture;

    impl Fixture {
        fn parse(body: &str) -> Result<Manifest, Error> {
            Manifest::parse(body, Revision::External)
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
             \"active\"\n"
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

    #[test]
    fn expands_explicit_mcp_groups_without_implicitly_allowing_merge() {
        for group in ["read", "comments", "issues", "patches", "reviews"] {
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
            let allowlist = McpAllowlist::from_entries(vec![method.clone()])
                .expect("registered tool should be accepted");
            assert!(allowlist.allows(method));
            assert!(
                method == "merge_patch"
                    || ["read", "comments", "issues", "patches", "reviews"]
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
                 \"active\"\n"
            )),
            Err(Error::UnknownMember(_))
        ));
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
             \"active\"\n[members.maya]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
             \"active\"\n"
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
             \"active\"\n[members.maya]\nsigning_did = \"{MAYA}\"\ngeneration = 1\nstate = \
             \"active\"\n"
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
             \"active\"\n"
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
             \"inactive\"\n"
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
             \"active\"\n"
        ))
        .expect("bindings should parse");
        assert!(matches!(
            active.validate_did(Member::Nico, maya),
            Err(Error::DidMismatch { .. })
        ));
    }
}
