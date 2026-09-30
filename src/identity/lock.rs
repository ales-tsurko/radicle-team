//! Exclusive local ownership of a member identity.
//!
//! [`MemberLock`] acquires an operating-system file lock inside one [`MemberHome`]. The application
//! holds it from before identity loading until the watcher stops, preventing two local processes
//! from using the same member home and reacting to an event twice.
//!
//! The lock coordinates processes on one filesystem only. It intentionally does not claim to fence
//! copies of the same private key on other machines, which the identity design declares
//! unsupported.

use std::{fs, io, path::Path};

use crate::home::MemberHome;

/// Exclusive ownership of one member home for the lifetime of a process.
#[derive(Debug)]
pub struct MemberLock {
    _file: fs::File,
}

impl MemberLock {
    /// Acquires the selected member home's process lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be opened or another process already holds the
    /// lock.
    pub fn acquire(home: &MemberHome) -> Result<Self, Error> {
        home.create()
            .map_err(|error| Error::Home(error.to_string()))?;
        let path = home.process_lock();
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone().into(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(Error::AlreadyRunning(home.path().into())),
            Err(fs::TryLockError::Error(source)) => Err(Error::Io {
                path: path.into(),
                source,
            }),
        }
    }
}

/// Error acquiring exclusive ownership of a member home.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Preparing the member directory failed.
    #[error("failed to prepare the member directory: {0}")]
    Home(String),
    /// Creating the member directory or lock file failed.
    #[error("failed to open member process lock '{path}': {source}")]
    Io {
        /// Lock file path.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// Another process already owns this member home.
    #[error("member home '{0}' is already in use by another process")]
    AlreadyRunning(Box<Path>),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{home::MemberHome, identity::MemberLock};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("radicle-team-lock-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_a_second_lock() {
        let directory = TestDirectory::new();
        let home = MemberHome::at(&directory.0);
        let _lock = MemberLock::acquire(&home).expect("first lock should be acquired");

        MemberLock::acquire(&home).expect_err("second lock should be rejected");
    }
}
