//! Linux helper lifecycle for embedded Zerobox execution.

use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::home::MemberHome;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub(super) fn dispatch() {
    zerobox::arg0::dispatch_linux_sandbox_helper();
    INITIALIZED.store(true, Ordering::Release);
}

/// Temporary executable alias used for Zerobox helper re-entry.
#[derive(Debug)]
pub(super) struct Helper {
    _directory: tempfile::TempDir,
    executable: PathBuf,
}

impl Helper {
    pub(super) fn prepare(home: &MemberHome) -> Result<Self, Error> {
        Self::ensure_available()?;
        fs::create_dir_all(home.path()).map_err(|source| Error::Io {
            path: home.path().into(),
            source,
        })?;
        let directory = Self::directory(home)?;
        let executable = directory.path().join("zerobox-linux-sandbox");
        Self::install(&executable)?;

        Ok(Self {
            _directory: directory,
            executable,
        })
    }

    /// Rejects an environment that cannot launch a sandboxed process at all.
    fn ensure_available() -> Result<(), Error> {
        Self::ensure_initialized()?;
        Self::ensure_bubblewrap()
    }

    fn ensure_initialized() -> Result<(), Error> {
        INITIALIZED
            .load(Ordering::Acquire)
            .then_some(())
            .ok_or(Error::Uninitialized)
    }

    /// Fails before any command runs when Zerobox has no bubblewrap to launch it with.
    ///
    /// Without this, the first sandboxed command dies in the re-executed helper process and the
    /// caller only sees an exit status, so a missing package looks like a broken agent. A
    /// `bwrap` beside the executable also counts, so a bundled distribution is not rejected.
    fn ensure_bubblewrap() -> Result<(), Error> {
        if zerobox_sandboxing::find_system_bwrap_in_path().is_some() {
            return Ok(());
        }
        let executable = std::env::current_exe().map_err(|source| Error::Io {
            path: Path::new("/proc/self/exe").into(),
            source,
        })?;
        if executable
            .parent()
            .is_some_and(|directory| directory.join("bwrap").is_file())
        {
            Ok(())
        } else {
            Err(Error::BubblewrapMissing)
        }
    }

    fn directory(home: &MemberHome) -> Result<tempfile::TempDir, Error> {
        tempfile::Builder::new()
            .prefix("sandbox-helper-")
            .tempdir_in(home.path())
            .map_err(|source| Error::Io {
                path: home.path().into(),
                source,
            })
    }

    fn install(executable: &Path) -> Result<(), Error> {
        let current = std::env::current_exe().map_err(|source| Error::Io {
            path: Path::new("/proc/self/exe").into(),
            source,
        })?;
        symlink(current, executable).map_err(|source| Error::Io {
            path: executable.into(),
            source,
        })
    }

    pub(super) fn executable(&self) -> &Path {
        &self.executable
    }
}

/// Error preparing the embedded Linux helper.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The embedding executable omitted early helper dispatch.
    #[error(
        "Linux sandbox helper is uninitialized; call \
         radicle_team::sandbox::dispatch_linux_helper() as the first statement in main"
    )]
    Uninitialized,
    /// Bubblewrap is absent, so Zerobox cannot launch a sandboxed process.
    #[error(
        "bubblewrap is required for agent sandboxing on Linux; install it so that 'bwrap' is on \
         PATH, for example 'sudo apt-get install bubblewrap'"
    )]
    BubblewrapMissing,
    /// A helper path could not be prepared.
    #[error("failed to prepare Linux sandbox helper path '{path}': {source}")]
    Io {
        /// Path being prepared.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
}
