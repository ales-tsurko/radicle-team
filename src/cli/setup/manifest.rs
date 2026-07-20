//! Safe manifest editing.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{fs, io, io::Write as _, path::Path};

use radicle::identity::Did;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

use super::Error;
use crate::{Application, Setup, SetupStatus, manifest::Manifest, member::Member};

#[derive(Debug, Clone, Copy)]
pub(super) struct ManifestEntry {
    pub(super) member: Member,
    pub(super) did: Did,
    pub(super) generation: u64,
}

impl ManifestEntry {
    pub(super) const fn new(application: &Application, setup: Setup) -> Self {
        Self {
            member: application.member(),
            did: setup.did(),
            generation: setup.status().generation(),
        }
    }

    pub(super) fn prepare(self, path: &Path, status: SetupStatus) -> Result<bool, Error> {
        let existing = match fs::read_to_string(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let create = existing.is_none();
        let contents = match status {
            SetupStatus::ManifestMissing { .. } if existing.is_none() => {
                format!("version = 1\n\n{self}\n")
            }
            SetupStatus::ManifestMissing { .. } | SetupStatus::BindingMissing { .. } => {
                self.extend(existing.as_deref().ok_or(Error::MissingManifest)?)?
            }
            SetupStatus::Ready { .. } => return Err(Error::ReadyManifest),
        };
        if existing.as_deref() == Some(contents.as_str()) {
            return Ok(false);
        }
        Manifest::validate_candidate(&contents, self.member, self.did)?;
        Self::write(path, &contents, create)?;

        Ok(true)
    }

    fn extend(self, contents: &str) -> Result<String, Error> {
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

    fn table(self) -> Result<Table, Error> {
        let mut binding = Table::new();
        let _previous = binding.insert("signing_did", value(self.did.to_string()));
        let _previous = binding.insert("generation", value(self.generation()?));
        let _previous = binding.insert("state", value("active"));

        Ok(binding)
    }

    fn inline_table(self) -> Result<Value, Error> {
        let mut binding = InlineTable::new();
        let _previous = binding.insert("signing_did", self.did.to_string().into());
        let _previous = binding.insert("generation", self.generation()?.into());
        let _previous = binding.insert("state", "active".into());

        Ok(binding.into())
    }

    fn generation(self) -> Result<i64, Error> {
        Ok(i64::try_from(self.generation)?)
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
             \"active\"",
            member = self.member,
            did = self.did,
            generation = self.generation,
        )
    }
}
