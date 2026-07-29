//! Safe manifest editing.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{collections::BTreeMap, fs, io, io::Write as _, path::Path};

use radicle::identity::Did;
use serde::Serialize;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::{
    Application, Setup,
    cli::setup::{Error, SecurityManifest},
    manifest::Manifest,
    member::Member,
};

#[derive(Debug, Clone)]
pub(crate) struct ManifestEntry {
    pub(super) member: Member,
    pub(super) did: Did,
    pub(super) generation: u64,
}

impl ManifestEntry {
    pub(super) fn new(application: &Application, setup: &Setup) -> Self {
        Self {
            member: application.member(),
            did: setup.did(),
            generation: setup.status().generation(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ManifestUpdate {
    Create {
        entry: ManifestEntry,
        security: SecurityManifest,
    },
    Extend(ManifestEntry),
}

#[derive(Serialize)]
struct SetupDocument<'a> {
    version: u32,
    security: SetupSecurity<'a>,
    members: BTreeMap<&'static str, SetupBinding>,
}

impl<'a> SetupDocument<'a> {
    fn new(entry: &ManifestEntry, security: &'a SecurityManifest) -> Self {
        Self {
            version: 1,
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
    generation: u64,
    state: &'static str,
    mcp_allow: &'static [&'static str],
}

impl ManifestUpdate {
    pub(super) const fn create(entry: ManifestEntry, security: SecurityManifest) -> Self {
        Self::Create { entry, security }
    }

    pub(super) const fn extend(entry: ManifestEntry) -> Self {
        Self::Extend(entry)
    }

    pub(super) const fn entry(&self) -> &ManifestEntry {
        match self {
            Self::Create { entry, .. } | Self::Extend(entry) => entry,
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
            Self::Create { entry, security } if existing.is_none() => {
                SetupDocument::new(entry, security).serialize()?
            }
            Self::Create { .. } => return Err(Error::ExistingManifest),
            Self::Extend(entry) => {
                entry.extend(existing.as_deref().ok_or(Error::MissingManifest)?)?
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
            Ok(binding) if binding.generation() == self.generation => {
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
                let _previous = members.insert(self.member.id(), Item::Table(self.table()?));
            }
            Item::Value(Value::InlineTable(members)) => {
                let _previous = members.insert(self.member.id(), self.inline_table()?);
            }
            _ => return Err(Error::InvalidMembers),
        }

        Ok(document.to_string())
    }

    fn table(&self) -> Result<Table, Error> {
        let mut binding = Table::new();
        let _previous = binding.insert("signing_did", value(self.did.to_string()));
        let _previous = binding.insert("generation", value(self.generation()?));
        let _previous = binding.insert("state", value("active"));
        let _previous = binding.insert("mcp_allow", value(self.mcp_allow()));

        Ok(binding)
    }

    fn inline_table(&self) -> Result<Value, Error> {
        let mut binding = InlineTable::new();
        let _previous = binding.insert("signing_did", self.did.to_string().into());
        let _previous = binding.insert("generation", self.generation()?.into());
        let _previous = binding.insert("state", "active".into());
        let _previous = binding.insert("mcp_allow", self.mcp_allow().into());

        Ok(binding.into())
    }

    fn generation(&self) -> Result<i64, Error> {
        Ok(i64::try_from(self.generation)?)
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

impl std::fmt::Display for ManifestEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "[members.{member}]\nsigning_did = \"{did}\"\ngeneration = {generation}\nstate = \
             \"active\"\nmcp_allow = [",
            member = self.member,
            did = self.did,
            generation = self.generation,
        )?;
        for (index, permission) in self.member.profile().mcp_allow.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "\"{permission}\"")?;
        }
        formatter.write_str("]")
    }
}
