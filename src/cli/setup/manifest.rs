//! Safe manifest editing.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{collections::BTreeMap, fmt, fs, io, io::Write as _, path::Path};

use radicle::identity::Did;
use serde::Serialize;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::{
    Application, Setup,
    cli::setup::{Error, SecurityManifest},
    manifest::{Manifest, TriggerAccess},
    member::Member,
};

#[derive(Debug, Clone)]
pub(crate) struct ManifestEntry {
    pub(crate) member: Member,
    pub(crate) did: Did,
    /// Narrowed here rather than at each use, so rendering a binding cannot fail.
    ///
    /// TOML holds no integer wider than this, so a generation which does not fit is an entry that
    /// could never be written. Saying so once, where it can be reported, beats every renderer
    /// carrying a failure it has no way to express.
    pub(crate) generation: i64,
    pub(crate) provider: Option<String>,
}

impl ManifestEntry {
    pub(super) fn new(application: &Application, setup: &Setup) -> Result<Self, Error> {
        Ok(Self {
            member: application.member(),
            did: setup.did(),
            generation: i64::try_from(setup.status().generation())?,
            provider: None,
        })
    }

    /// Records which model provider this member is to run on.
    #[must_use]
    pub(super) fn with_provider(mut self, provider: Option<String>) -> Self {
        self.provider = provider;
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ManifestUpdate {
    Create {
        entry: ManifestEntry,
        security: SecurityManifest,
        triggered_by: TriggerAccess,
    },
    Extend(ManifestEntry),
    Provider(ManifestEntry),
}

#[derive(Serialize)]
struct SetupDocument<'a> {
    version: u32,
    triggered_by: &'a TriggerAccess,
    security: SetupSecurity<'a>,
    members: BTreeMap<&'static str, SetupBinding>,
}

impl<'a> SetupDocument<'a> {
    fn new(
        entry: &ManifestEntry,
        security: &'a SecurityManifest,
        triggered_by: &'a TriggerAccess,
    ) -> Self {
        Self {
            version: 1,
            triggered_by,
            security: SetupSecurity {
                repository: security.repository,
                recipients: [&security.recipient],
            },
            members: BTreeMap::from([(
                entry.member.id(),
                SetupBinding {
                    signing_did: entry.did,
                    generation: entry.generation,
                    state: "active",
                    mcp_allow: entry.member.profile().mcp_allow,
                    provider: entry.provider.clone(),
                },
            )]),
        }
    }

    fn serialize(&self) -> Result<String, Error> {
        toml_edit::ser::to_string_pretty(self).map_err(Error::from)
    }
}

#[derive(Serialize)]
struct SetupSecurity<'a> {
    repository: radicle::identity::RepoId,
    recipients: [&'a str; 1],
}

#[derive(Serialize)]
struct SetupBinding {
    signing_did: Did,
    generation: i64,
    state: &'static str,
    mcp_allow: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

impl ManifestUpdate {
    pub(super) const fn create(
        entry: ManifestEntry,
        security: SecurityManifest,
        triggered_by: TriggerAccess,
    ) -> Self {
        Self::Create {
            entry,
            security,
            triggered_by,
        }
    }

    pub(super) const fn extend(entry: ManifestEntry) -> Self {
        Self::Extend(entry)
    }

    pub(super) const fn entry(&self) -> &ManifestEntry {
        match self {
            Self::Create { entry, .. } | Self::Extend(entry) | Self::Provider(entry) => entry,
        }
    }

    pub(super) fn prepare(&self, path: &Path) -> Result<bool, Error> {
        let existing = match fs::read_to_string(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let create = existing.is_none();
        let contents = match self {
            Self::Create {
                entry,
                security,
                triggered_by,
            } if existing.is_none() => {
                SetupDocument::new(entry, security, triggered_by).serialize()?
            }
            Self::Create { .. } => return Err(Error::ExistingManifest),
            Self::Extend(entry) => {
                entry.extend(existing.as_deref().ok_or(Error::MissingManifest)?)?
            }
            Self::Provider(entry) => {
                entry.change_provider(existing.as_deref().ok_or(Error::MissingManifest)?)?
            }
        };
        if existing.as_deref() == Some(contents.as_str()) {
            return Ok(false);
        }
        let entry = self.entry();
        Manifest::validate_candidate(&contents, entry.member, entry.did)?;
        ManifestEntry::write(path, &contents, create)?;

        Ok(true)
    }
}

impl ManifestEntry {
    fn change_provider(&self, contents: &str) -> Result<String, Error> {
        let binding = Manifest::validate_candidate(contents, self.member, self.did)?;
        if binding.generation() != self.generation.unsigned_abs() {
            return Err(Error::ConflictingBinding(self.member));
        }
        let mut document = contents.parse::<DocumentMut>()?;
        let member = document
            .get_mut("members")
            .and_then(Item::as_table_like_mut)
            .and_then(|members| members.get_mut(self.member.id()))
            .and_then(Item::as_table_like_mut)
            .ok_or(Error::InvalidMembers)?;
        let _previous = member.insert(
            "provider",
            value(self.provider.as_deref().ok_or(Error::MissingProposal)?),
        );
        Ok(document.to_string())
    }

    fn extend(&self, contents: &str) -> Result<String, Error> {
        let mut document = contents.parse::<DocumentMut>()?;
        let has_member = document
            .get("members")
            .is_some_and(|members| match members {
                Item::Table(members) => members.contains_key(self.member.id()),
                Item::Value(Value::InlineTable(members)) => members.contains_key(self.member.id()),
                _ => false,
            });
        match Manifest::validate_candidate(contents, self.member, self.did) {
            Ok(binding) if binding.generation() == self.generation.unsigned_abs() => {
                return Ok(contents.to_owned());
            }
            Ok(_)
            | Err(crate::manifest::Error::Inactive(_))
            | Err(crate::manifest::Error::DidMismatch { .. }) => {
                return Err(Error::ConflictingBinding(self.member));
            }
            Err(crate::manifest::Error::Parse(_)) if has_member => {
                return Err(Error::ConflictingBinding(self.member));
            }
            Err(crate::manifest::Error::Missing { .. }) => {}
            Err(error) => return Err(error.into()),
        }

        let members = document.get_mut("members").ok_or(Error::MissingManifest)?;
        match members {
            Item::Table(members) => {
                let _previous = members.insert(self.member.id(), Item::Table(self.table()));
            }
            Item::Value(Value::InlineTable(members)) => {
                let _previous = members.insert(self.member.id(), self.inline_table());
            }
            _ => return Err(Error::InvalidMembers),
        }

        Ok(document.to_string())
    }

    fn table(&self) -> Table {
        let mut binding = Table::new();
        let _previous = binding.insert("signing_did", value(self.did.to_string()));
        let _previous = binding.insert("generation", value(self.generation));
        let _previous = binding.insert("state", value("active"));
        let _previous = binding.insert("mcp_allow", value(self.mcp_allow()));
        if let Some(provider) = &self.provider {
            let _previous = binding.insert("provider", value(provider));
        }

        binding
    }

    fn inline_table(&self) -> Value {
        let mut binding = InlineTable::new();
        let _previous = binding.insert("signing_did", self.did.to_string().into());
        let _previous = binding.insert("generation", self.generation.into());
        let _previous = binding.insert("state", "active".into());
        let _previous = binding.insert("mcp_allow", self.mcp_allow().into());
        if let Some(provider) = &self.provider {
            let _previous = binding.insert("provider", provider.as_str().into());
        }

        binding.into()
    }

    fn mcp_allow(&self) -> Array {
        self.member.profile().mcp_allow.iter().copied().collect()
    }

    fn write(path: &Path, contents: &str, create: bool) -> Result<(), io::Error> {
        if create {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(path)?;
            return file.write_all(contents.as_bytes());
        }

        let metadata = Self::regular_file_metadata(path)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement
            .as_file()
            .set_permissions(metadata.permissions())?;
        replacement.write_all(contents.as_bytes())?;
        replacement.as_file().sync_all()?;

        // If a symlink appears after this check, the atomic rename replaces the link itself rather
        // than following it.
        let _metadata = Self::regular_file_metadata(path)?;
        replacement
            .persist(path)
            .map(|_file| ())
            .map_err(|error| error.error)
    }

    fn symlink_error(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to write symbolic link '{}'", path.display()),
        )
    }

    fn regular_file_metadata(path: &Path) -> Result<fs::Metadata, io::Error> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            Err(Self::symlink_error(path))
        } else {
            Ok(metadata)
        }
    }
}

// Rendered from the same table the manifest is written from, so what an operator is told to paste
// is what setup would have written. Two renderers is how a new field reaches the file and not the
// instructions.
impl fmt::Display for ManifestEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let binding = self.table().to_string();

        write!(
            formatter,
            "[members.{}]\n{}",
            self.member,
            binding.trim_end()
        )
    }
}
