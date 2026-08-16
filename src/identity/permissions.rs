//! Filesystem checks shared by identity providers and manifest loaders.
//!
//! [`Permissions`] creates private member directories and validates sensitive files before they
//! enter the identity system. Private keys must deny group and other access, while administrator
//! manifest files may be readable but not writable by them. On Unix, both must belong to the
//! current user.
//!
//! Keeping these checks here gives managed keys, imported Radicle homes, and external manifests
//! files one consistent fail-closed policy.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::{
    fs::{self, File},
    io::{self, Write as _},
    path::Path,
};

pub(crate) struct Permissions;

impl Permissions {
    pub(crate) fn private_file(path: &Path) -> Result<(), Error> {
        Self::file(path, FilePolicy::Private)
    }

    pub(crate) fn open_private(path: &Path) -> Result<File, Error> {
        let file = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map(File::from)
        .map_err(|source| Error::Io {
            path: path.into(),
            source: io::Error::from_raw_os_error(source.raw_os_error()),
        })?;
        let metadata = file.metadata().map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        Self::validate(path, &metadata, FilePolicy::Private)?;
        Ok(file)
    }

    pub(crate) fn administrator_file(path: &Path) -> Result<(), Error> {
        Self::file(path, FilePolicy::Administrator)
    }

    fn file(path: &Path, _policy: FilePolicy) -> Result<(), Error> {
        let metadata = Self::metadata(path)?;
        Self::validate(path, &metadata, _policy)
    }

    fn validate(path: &Path, metadata: &fs::Metadata, _policy: FilePolicy) -> Result<(), Error> {
        if !metadata.file_type().is_file() {
            return Err(Error::FileType { path: path.into() });
        }

        #[cfg(unix)]
        {
            Self::owner(path, metadata)?;
            let mode = metadata.mode() & 0o777;
            if mode & _policy.denied_mode() != 0 {
                return Err(Error::Mode {
                    path: path.into(),
                    mode,
                    expected: _policy.expected(),
                });
            }
        }

        Ok(())
    }

    /// Writes `contents` to `path` so that only its owner can ever read it.
    ///
    /// Atomic, so a write interrupted part-way leaves the previous contents rather than a truncated
    /// file, and private from the moment the bytes exist rather than from the moment a later
    /// `chmod` lands. Both matter most for the file this exists for: a stored secret.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written, secured, or moved into place.
    pub(crate) fn write_private(path: &Path, contents: &str) -> Result<(), Error> {
        Self::persist(path, contents, Some(0o600))
    }

    /// Writes `contents` to `path`, replacing whatever is there in one step.
    ///
    /// For a file which is not a secret. Same durability as [`Permissions::write_private`] without
    /// narrowing the mode: the file keeps whatever the umask gives it.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written or moved into place.
    pub(crate) fn replace(path: &Path, contents: &str) -> Result<(), Error> {
        Self::persist(path, contents, None)
    }

    // Renames into place, so an interrupted write leaves the previous contents rather than a
    // truncated file, a symbolic link at `path` is replaced rather than written through, and the
    // mode is on the bytes from the moment they exist rather than from a later `chmod`.
    fn persist(path: &Path, contents: &str, _mode: Option<u32>) -> Result<(), Error> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let io = |source| Error::Io {
            path: path.into(),
            source,
        };
        let mut replacement = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
        #[cfg(unix)]
        if let Some(mode) = _mode {
            replacement
                .as_file()
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(io)?;
        }
        replacement.write_all(contents.as_bytes()).map_err(io)?;
        replacement.as_file().sync_all().map_err(io)?;

        replacement
            .persist(path)
            .map(|_file| ())
            .map_err(|error| io(error.error))
    }

    pub(crate) fn secure_directory(path: &Path) -> Result<(), Error> {
        fs::create_dir_all(path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;

        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Error::Io {
                path: path.into(),
                source,
            }
        })?;

        Ok(())
    }

    fn metadata(path: &Path) -> Result<fs::Metadata, Error> {
        fs::symlink_metadata(path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }

    #[cfg(unix)]
    fn owner(path: &Path, metadata: &fs::Metadata) -> Result<(), Error> {
        let expected = rustix::process::geteuid().as_raw();
        let actual = metadata.uid();
        if actual == expected {
            Ok(())
        } else {
            Err(Error::Owner {
                path: path.into(),
                expected,
                actual,
            })
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FilePolicy {
    Private,
    Administrator,
}

#[cfg(unix)]
impl FilePolicy {
    const fn denied_mode(self) -> u32 {
        match self {
            Self::Private => 0o077,
            Self::Administrator => 0o022,
        }
    }

    const fn expected(self) -> &'static str {
        match self {
            Self::Private => "no group or other access",
            Self::Administrator => "not writable by group or others",
        }
    }
}

/// Error validating identity-related filesystem permissions.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem metadata or permissions could not be accessed.
    #[error("failed to access '{path}': {source}")]
    Io {
        /// Path being validated.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A sensitive path is not a regular file.
    #[error("'{path}' is not a regular file")]
    FileType {
        /// Path being validated.
        path: Box<Path>,
    },
    #[cfg(unix)]
    /// A path is not owned by the current user.
    #[error("'{path}' is owned by user {actual}, expected user {expected}")]
    Owner {
        /// Path being validated.
        path: Box<Path>,
        /// Expected user ID.
        expected: u32,
        /// Actual user ID.
        actual: u32,
    },
    #[cfg(unix)]
    /// A path grants unsafe access.
    #[error("unsafe permissions {mode:o} on '{path}', expected {expected}")]
    Mode {
        /// Path being validated.
        path: Box<Path>,
        /// Actual Unix mode.
        mode: u32,
        /// Human-readable permission requirement.
        expected: &'static str,
    },
}
