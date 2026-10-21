//! Exclusive local ownership of a member identity.
//!
//! [`MemberLock`] acquires an operating-system file lock inside one [`MemberHome`]. The application
//! holds it from before identity loading until the watcher stops, preventing two local processes
//! from using the same member home and reacting to an event twice.
//!
//! The lock coordinates processes on one filesystem only. It intentionally does not claim to fence
//! copies of the same private key on other machines, which the identity design declares
//! unsupported.

use std::{
    fs,
    io::{self, Read as _, Write as _},
    path::Path,
};

use crate::home::MemberHome;

/// Exclusive ownership of one member home for the lifetime of a process.
#[derive(Debug)]
pub struct MemberLock {
    file: fs::File,
    session: String,
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
        let mut file = fs::OpenOptions::new()
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
            Ok(()) => {
                let session = format!("{:032x}", rand::random::<u128>());
                file.set_len(0)
                    .and_then(|()| file.write_all(session.as_bytes()))
                    .map_err(|source| Error::Io {
                        path: path.into(),
                        source,
                    })?;
                Ok(Self { file, session })
            }
            Err(fs::TryLockError::WouldBlock) => Err(Error::AlreadyRunning(home.path().into())),
            Err(fs::TryLockError::Error(source)) => Err(Error::Io {
                path: path.into(),
                source,
            }),
        }
    }

    /// Returns the identifier of this acquisition, distinct from every previous owner.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Reads the current lock owner's session without creating or changing the lock file.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing lock cannot be read or probed.
    pub fn owner(home: &MemberHome) -> Result<Option<String>, Error> {
        let path = home.process_lock();
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Io {
                    path: path.into(),
                    source,
                });
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(None),
            Err(fs::TryLockError::WouldBlock) => {
                let mut session = String::new();
                file.read_to_string(&mut session)
                    .map_err(|source| Error::Io {
                        path: path.into(),
                        source,
                    })?;
                Ok(Some(session))
            }
            Err(fs::TryLockError::Error(source)) => Err(Error::Io {
                path: path.into(),
                source,
            }),
        }
    }
}

impl Drop for MemberLock {
    fn drop(&mut self) {
        // A concurrently spawned child can briefly inherit this open file description before
        // exec closes it. Release ownership now instead of waiting for that duplicate to close.
        let _unlocked = self.file.unlock();
    }
}

/// Error acquiring exclusive ownership of a member home.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Preparing the member directory failed.
    #[error("failed to prepare the member directory: {0}")]
    Home(String),
    /// Creating the member directory or lock file failed.
    #[error("failed to access member process lock '{path}': {source}")]
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

    #[test]
    fn releases_ownership_while_an_inherited_handle_stays_open() {
        let directory = TestDirectory::new();
        let home = MemberHome::at(&directory.0);
        let lock = MemberLock::acquire(&home).expect("lock should acquire");
        let inherited = lock
            .file
            .try_clone()
            .expect("inherited handle should duplicate");

        drop(lock);
        let replacement = MemberLock::acquire(&home)
            .expect("dropping the owner must release its lock before inherited handles close");
        drop(inherited);
        MemberLock::acquire(&home).expect_err("the replacement must retain ownership");
        drop(replacement);
    }

    #[test]
    fn observes_only_a_held_lock_and_distinguishes_each_acquisition() {
        let directory = TestDirectory::new();
        let home = MemberHome::at(&directory.0);
        assert_eq!(
            MemberLock::owner(&home).expect("missing lock should be readable"),
            None
        );
        assert!(!home.process_lock().exists());
        let first = MemberLock::acquire(&home).expect("lock should acquire");
        let session = first.session().to_owned();
        assert_eq!(
            MemberLock::owner(&home).expect("owner should read"),
            Some(session.clone())
        );
        drop(first);
        assert_eq!(
            MemberLock::owner(&home).expect("stale file should read"),
            None
        );
        let second = MemberLock::acquire(&home).expect("lock should acquire again");
        assert_ne!(second.session(), session);
        assert_eq!(
            MemberLock::owner(&home).expect("new owner should read"),
            Some(second.session().into())
        );
    }
}
