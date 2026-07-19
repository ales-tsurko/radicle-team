//! Filesystem checks shared by identity providers and configuration loaders.
//!
//! [`Permissions`] creates private member directories and validates sensitive files before they
//! enter the identity system. Private keys must deny group and other access, while administrator
//! configuration files may be readable but not writable by them. On Unix, both must belong to the
//! current user.
//!
//! Keeping these checks here gives managed keys, imported Radicle homes, and external configuration
//! files one consistent fail-closed policy.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::{fs, io, path::Path};

pub(crate) struct Permissions;

impl Permissions {
    pub(super) fn private_file(path: &Path) -> Result<(), Error> {
        Self::file(path, FilePolicy::Private)
    }

    pub(crate) fn administrator_file(path: &Path) -> Result<(), Error> {
        Self::file(path, FilePolicy::Administrator)
    }

    fn file(path: &Path, _policy: FilePolicy) -> Result<(), Error> {
        let metadata = Self::metadata(path)?;

        #[cfg(unix)]
        {
            Self::owner(path, &metadata)?;
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

    pub(super) fn secure_directory(path: &Path) -> Result<(), Error> {
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
