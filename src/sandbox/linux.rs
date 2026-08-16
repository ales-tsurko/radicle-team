//! Linux helper lifecycle for embedded Zerobox execution.

use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::home::MemberHome;

// Says that something in this process called `dispatch`, not that this executable dispatches before
// its arguments are parsed. A libtest binary calling it from a test body sets it and still cannot
// serve as the helper, because libtest owns `main`: the re-entered process rejects Zerobox's
// arguments instead of becoming the sandbox.
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
        let source = Self::available_source()?;
        let directory = Self::directory(home)?;
        let executable = Self::alias(directory.path(), &source)?;

        Ok(Self {
            _directory: directory,
            executable,
        })
    }

    /// Rejects an environment which cannot launch a sandboxed process at all, then reports the
    /// executable Zerobox re-enters as the helper.
    fn available_source() -> Result<PathBuf, Error> {
        Self::ensure_bubblewrap()?;

        Self::source()
    }

    /// Returns the executable Zerobox re-enters as the helper.
    ///
    /// The running one, which must therefore dispatch before it parses its arguments.
    #[cfg(not(test))]
    fn source() -> Result<PathBuf, Error> {
        INITIALIZED
            .load(Ordering::Acquire)
            .then_some(())
            .ok_or(Error::Uninitialized)?;

        std::env::current_exe().map_err(|source| Error::Io {
            path: Path::new("/proc/self/exe").into(),
            source,
        })
    }

    /// Returns the `sandbox-helper` binary, built beside the test binary that is running.
    ///
    /// The running executable cannot serve here: libtest owns `main`, so the re-entered process
    /// would reject Zerobox's arguments instead of becoming the sandbox.
    #[cfg(test)]
    fn source() -> Result<PathBuf, Error> {
        let current = std::env::current_exe().map_err(|source| Error::Io {
            path: Path::new("/proc/self/exe").into(),
            source,
        })?;
        let helper = current
            .parent()
            .and_then(Path::parent)
            .ok_or(Error::HelperMissing {
                path: current.clone().into(),
            })?
            .join("sandbox-helper");

        if helper.is_file() {
            Ok(helper)
        } else {
            Err(Error::HelperMissing {
                path: helper.into(),
            })
        }
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

    /// Returns a directory of the helper's own inside the member's, creating both.
    fn directory(home: &MemberHome) -> Result<tempfile::TempDir, Error> {
        fs::create_dir_all(home.path()).map_err(|source| Error::Io {
            path: home.path().into(),
            source,
        })?;

        tempfile::Builder::new()
            .prefix("sandbox-helper-")
            .tempdir_in(home.path())
            .map_err(|source| Error::Io {
                path: home.path().into(),
                source,
            })
    }

    /// Links `source` under the name Zerobox re-enters, and returns where that link is.
    fn alias(directory: &Path, source: &Path) -> Result<PathBuf, Error> {
        let executable = directory.join("zerobox-linux-sandbox");
        symlink(source, &executable).map_err(|error| Error::Io {
            path: executable.clone().into(),
            source: error,
        })?;

        Ok(executable)
    }

    pub(super) fn executable(&self) -> &Path {
        &self.executable
    }
}

/// Error preparing the embedded Linux helper.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The embedding executable omitted early helper dispatch.
    #[cfg(not(test))]
    #[error(
        "Linux sandbox helper is uninitialized; call \
         radicle_team::sandbox::dispatch_linux_helper() as the first statement in main"
    )]
    Uninitialized,
    /// The `sandbox-helper` binary the tests use as the helper was not built.
    #[cfg(test)]
    #[error(
        "Linux sandbox helper '{path}' was not built; run the tests over the whole workspace, for \
         example 'cargo test --workspace'"
    )]
    HelperMissing {
        /// Where the helper was looked for.
        path: Box<Path>,
    },
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
