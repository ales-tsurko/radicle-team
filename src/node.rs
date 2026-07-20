//! Lifecycle management for a team member's Radicle node.
//!
//! [`NodeManager`](crate::node::NodeManager) starts `radicle-node` with the selected
//! [`Identity`](crate::identity::Identity), waits for its isolated control socket to become
//! responsive, and verifies that the node uses the expected identity. The child inherits only the
//! Radicle home and passphrase required by the identity provider. Its output is appended to the
//! member's node log.
//!
//! Each managed identity has its own Radicle home and therefore its own control socket, storage,
//! and databases. Radicle's default client profile does not open a listening TCP port, so multiple
//! members can run concurrently without port allocation.
//! [`NodeManager::stop`](crate::node::NodeManager::stop) requests graceful shutdown; dropping the
//! manager forcibly reaps a child that is still running. The CLI handles interruption and
//! termination signals, forwards graceful shutdown to the node, and then lets the manager reap it.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

use radicle::node::{Handle as _, Node, NodeId};

use crate::{home::MemberHome, identity::Identity, shutdown::Shutdown};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Owns one supervised `radicle-node` child process.
#[derive(Debug)]
pub struct NodeManager {
    child: ChildHandle,
    node: Node,
    log: PathBuf,
}

/// A process handle shared with signal supervision.
#[derive(Debug, Clone)]
pub(crate) struct ChildHandle {
    child: Arc<Mutex<Option<Child>>>,
}

impl ChildHandle {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child: Arc::new(Mutex::new(Some(child))),
        }
    }

    pub(crate) fn kill(&self) -> Result<(), io::Error> {
        let mut child = self.lock();
        if let Some(child) = child.as_mut()
            && child.try_wait()?.is_none()
        {
            child.kill()?;
        }

        Ok(())
    }

    pub(crate) fn wait(&self) -> Result<ExitStatus, io::Error> {
        loop {
            {
                let mut child = self.lock();
                let Some(process) = child.as_mut() else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "child process is not running",
                    ));
                };
                if let Some(status) = process.try_wait()? {
                    let _process = child.take();
                    return Ok(status);
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Child>> {
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl NodeManager {
    /// Starts and verifies the selected identity's Radicle node.
    ///
    /// # Errors
    ///
    /// Returns an error when another node is already running, the process cannot be started, it
    /// exits before becoming ready, or it reports an unexpected identity.
    pub fn start(identity: &Identity, home: &MemberHome) -> Result<Self, Error> {
        let mut manager = Self::spawn_manager(identity, home)?;
        manager.wait_until_ready(*identity.profile().id(), STARTUP_TIMEOUT)?;

        Ok(manager)
    }

    pub(crate) fn start_supervised(
        identity: &Identity,
        home: &MemberHome,
        shutdown: &Shutdown,
    ) -> Result<Self, Error> {
        shutdown.begin_start();
        let mut manager = match Self::spawn_manager(identity, home) {
            Ok(manager) => manager,
            Err(error) => {
                shutdown.cancel_start();
                return Err(error);
            }
        };
        shutdown.supervise(manager.node(), manager.child_handle());
        if let Err(error) = manager.wait_until_ready(*identity.profile().id(), STARTUP_TIMEOUT) {
            let cleanup = shutdown.finish_supervision(|| {
                if shutdown.shutdown_started() {
                    manager.reap()
                } else {
                    drop(manager);
                    Ok(())
                }
            });
            if let Err(cleanup) = cleanup {
                tracing::warn!(%cleanup, "failed to reap node after startup error");
            }
            return Err(error);
        }

        Ok(manager)
    }

    fn spawn_manager(identity: &Identity, home: &MemberHome) -> Result<Self, Error> {
        let node = Self::available_node(identity)?;
        let log = home.node_log();
        let child = Self::spawn(identity, &log)?;
        Ok(Self {
            child: ChildHandle::new(child),
            node,
            log,
        })
    }

    fn available_node(identity: &Identity) -> Result<Node, Error> {
        let socket = identity.profile().home().socket_default();
        let node = Node::new(&socket);
        if node.is_running() {
            return Err(Error::AlreadyRunning(socket.into()));
        }
        Self::ensure_socket_absent(&socket)?;

        Ok(node)
    }

    fn spawn(identity: &Identity, log: &Path) -> Result<Child, Error> {
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| Error::Log {
                path: log.to_path_buf(),
                source,
            })?;
        let error = output.try_clone().map_err(|source| Error::Log {
            path: log.to_path_buf(),
            source,
        })?;
        let mut command = Command::new("radicle-node");
        command
            .arg("--log-level")
            .arg("info")
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error));
        identity.configure_radicle_command(&mut command);
        command.spawn().map_err(Error::Spawn)
    }

    /// Returns the connected Radicle node handle.
    #[must_use]
    pub const fn node(&self) -> &Node {
        &self.node
    }

    /// Returns the managed child process handle.
    pub(crate) fn child_handle(&self) -> ChildHandle {
        self.child.clone()
    }

    /// Gracefully stops the child node and waits for it to exit.
    ///
    /// # Errors
    ///
    /// Returns an error when the shutdown request or process wait fails.
    pub fn stop(mut self) -> Result<(), Error> {
        self.node.clone().shutdown().map_err(Error::Control)?;
        self.reap_child()
    }

    pub(crate) fn stop_supervised(mut self, shutdown: &Shutdown) -> Result<(), Error> {
        if shutdown.shutdown_started() {
            return self.reap_child();
        }
        match self.node.clone().shutdown() {
            Ok(()) => {
                shutdown.mark_shutdown_started();
                self.reap_child()
            }
            Err(_) if shutdown.shutdown_started() => self.reap_child(),
            Err(error) => Err(Error::Control(error)),
        }
    }

    pub(crate) fn reap(mut self) -> Result<(), Error> {
        self.reap_child()
    }

    fn reap_child(&mut self) -> Result<(), Error> {
        let status = self.child.wait().map_err(Error::Wait)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Exited {
                status,
                log: self.log.clone(),
            })
        }
    }

    fn wait_until_ready(&mut self, expected: NodeId, timeout: Duration) -> Result<(), Error> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            let status = {
                let mut child = self.child.lock();
                let Some(child) = child.as_mut() else {
                    return Err(Error::NotRunning);
                };
                child.try_wait().map_err(Error::Wait)?
            };
            if let Some(status) = status {
                return Err(Error::Exited {
                    status,
                    log: self.log.clone(),
                });
            }
            if let Ok(actual) = self.node.nid() {
                return if actual == expected {
                    Ok(())
                } else {
                    Err(Error::IdentityMismatch { expected, actual })
                };
            }
            thread::sleep(POLL_INTERVAL);
        }

        Err(Error::StartupTimeout {
            timeout,
            log: self.log.clone(),
        })
    }

    fn ensure_socket_absent(socket: &Path) -> Result<(), Error> {
        match fs::symlink_metadata(socket) {
            Ok(_) => Err(Error::SocketOccupied(socket.into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Socket {
                path: socket.into(),
                source,
            }),
        }
    }
}

impl Drop for NodeManager {
    fn drop(&mut self) {
        let _killed = self.child.kill();
        let _status = self.child.wait();
    }
}

/// Error managing a Radicle node child process.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The manager no longer owns a running child.
    #[error("the Radicle node process is not running")]
    NotRunning,
    /// Another node already owns the identity's control socket.
    #[error("a Radicle node is already running at '{0}'")]
    AlreadyRunning(Box<Path>),
    /// The control socket could not be inspected.
    #[error("failed to inspect Radicle control socket '{path}': {source}")]
    Socket {
        /// Control socket path.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A control socket exists but its node did not respond.
    #[error(
        "Radicle control socket '{0}' exists but is unresponsive; verify its node has stopped \
         before removing it"
    )]
    SocketOccupied(Box<Path>),
    /// The node log could not be opened.
    #[error("failed to open Radicle node log '{path}': {source}")]
    Log {
        /// Log path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The node process could not be started.
    #[error("failed to start radicle-node: {0}")]
    Spawn(io::Error),
    /// The node process could not be inspected or reaped.
    #[error("failed to wait for radicle-node: {0}")]
    Wait(io::Error),
    /// The child exited unexpectedly.
    #[error("radicle-node exited with {status}; see '{log}'")]
    Exited {
        /// Exit status.
        status: ExitStatus,
        /// Node log path.
        log: PathBuf,
    },
    /// The control socket did not become ready in time.
    #[error("radicle-node did not become ready within {timeout:?}; see '{log}'")]
    StartupTimeout {
        /// Startup timeout.
        timeout: Duration,
        /// Node log path.
        log: PathBuf,
    },
    /// The node uses a different identity from the loaded profile.
    #[error("Radicle node identity {actual} does not match expected identity {expected}")]
    IdentityMismatch {
        /// Expected identity.
        expected: NodeId,
        /// Actual node identity.
        actual: NodeId,
    },
    /// A control-socket operation failed.
    #[error("Radicle node control error: {0}")]
    Control(radicle::node::Error),
}

#[cfg(all(test, unix, not(miri)))]
mod tests {
    use std::{fs, path::PathBuf, process::Command, time::Duration};

    use radicle::node::{Node, NodeId};

    use crate::node::{ChildHandle, Error, NodeManager};

    const NODE: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[test]
    fn preserves_an_existing_unresponsive_socket() {
        let socket = PathBuf::from("target/tests/node/existing-control.sock");
        fs::create_dir_all(socket.parent().expect("socket should have a parent"))
            .expect("fixture directory should be created");
        fs::write(&socket, "occupied").expect("socket placeholder should be written");

        assert!(matches!(
            NodeManager::ensure_socket_absent(&socket),
            Err(Error::SocketOccupied(_))
        ));
        assert!(socket.exists());
        fs::remove_file(socket).expect("socket placeholder should be removed");
    }

    #[test]
    fn reports_a_child_that_exits_during_startup() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("test child should start");
        let socket = std::env::temp_dir().join(format!(
            "radicle-team-missing-socket-{}",
            std::process::id()
        ));
        let mut manager = NodeManager {
            child: ChildHandle::new(child),
            node: Node::new(socket),
            log: PathBuf::from("node.log"),
        };
        let expected = NODE.parse::<NodeId>().expect("node ID should parse");

        assert!(matches!(
            manager.wait_until_ready(expected, Duration::from_secs(1)),
            Err(Error::Exited { .. })
        ));
    }
}
