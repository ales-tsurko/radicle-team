//! Lifecycle management for a team member's Radicle node.
//!
//! [`NodeManager`] starts `radicle-node` with the selected [`Identity`], waits for its isolated
//! control socket to become responsive, and verifies that the node uses the expected identity. The
//! child inherits only the Radicle home and passphrase required by the identity provider. Its
//! output is appended to the member's node log.
//!
//! Each managed identity has its own Radicle home and therefore its own control socket, storage,
//! and databases. Radicle's default client profile does not open a listening TCP port, so multiple
//! members can run concurrently without port allocation.
//! [`NodeManager::stop`](crate::node::NodeManager::stop) requests graceful shutdown. Dropping the
//! manager forcibly terminates a child that is still running. The CLI handles interruption and
//! termination signals, forwards graceful shutdown to the node, and then lets the manager reap it.

use std::{
    fs, io,
    os::unix::fs::FileTypeExt as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use radicle::{
    identity::RepoId,
    node::{
        Command as NodeCommand, ConnectOptions, ConnectResult, Handle as _, Node, NodeId, Session,
        State, Success, config::ConnectAddress,
    },
};
use tokio::process::{Child, Command};

use crate::{home::MemberHome, identity::Identity, shutdown::Shutdown};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const READINESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Owns one supervised `radicle-node` child process.
#[derive(Debug)]
pub struct NodeManager {
    child: ChildHandle,
    node: Node,
    socket: PathBuf,
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
            child.start_kill()?;
        }

        Ok(())
    }

    pub(crate) async fn wait(&self) -> Result<ExitStatus, io::Error> {
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
            tokio::time::sleep(POLL_INTERVAL).await;
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
    pub async fn start(identity: &Identity, home: &MemberHome) -> Result<Self, Error> {
        let mut manager = Self::spawn_manager(identity, home).await?;
        match manager
            .wait_until_ready(*identity.profile().id(), STARTUP_TIMEOUT)
            .await
        {
            Ok(()) => Ok(manager),
            Err(error) => {
                if let Err(cleanup) = manager.terminate_and_reap().await {
                    tracing::warn!(%cleanup, "failed to reap node after startup error");
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn start_supervised(
        identity: &Identity,
        home: &MemberHome,
        shutdown: &Shutdown,
    ) -> Result<Self, Error> {
        let mut manager = Self::spawn_supervised(identity, home, shutdown).await?;
        shutdown.supervise(manager.node(), manager.child_handle());
        match manager
            .wait_until_ready(*identity.profile().id(), STARTUP_TIMEOUT)
            .await
        {
            Ok(()) => Ok(manager),
            Err(error) => Err(Self::cleanup_failed_start(manager, shutdown, error).await),
        }
    }

    async fn spawn_supervised(
        identity: &Identity,
        home: &MemberHome,
        shutdown: &Shutdown,
    ) -> Result<Self, Error> {
        shutdown.begin_start();
        Self::spawn_manager(identity, home).await.inspect_err(|_| {
            shutdown.cancel_start();
        })
    }

    async fn cleanup_failed_start(manager: Self, shutdown: &Shutdown, error: Error) -> Error {
        let cleanup = if shutdown.shutdown_started() {
            manager.reap().await
        } else {
            manager.terminate_and_reap().await
        };
        shutdown.release_supervision();
        if let Err(cleanup) = cleanup {
            tracing::warn!(%cleanup, "failed to reap node after startup error");
        }
        error
    }

    async fn spawn_manager(identity: &Identity, home: &MemberHome) -> Result<Self, Error> {
        let socket = identity.profile().home().socket_default();
        let check_socket = socket.clone();
        let node = tokio::task::spawn_blocking(move || Self::available_node(check_socket))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        let log = home.node_log();
        let child = Self::spawn(identity, &log).await?;
        Ok(Self {
            child: ChildHandle::new(child),
            node,
            socket,
            log,
        })
    }

    fn available_node(socket: PathBuf) -> Result<Node, Error> {
        let node = Node::new(&socket);
        if node.is_running() {
            return Err(Error::AlreadyRunning(socket.into_boxed_path()));
        }
        Self::ensure_socket_absent(&socket)?;

        Ok(node)
    }

    async fn spawn(identity: &Identity, log: &Path) -> Result<Child, Error> {
        let output = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .await
            .map_err(|source| Error::Log {
                path: log.to_path_buf(),
                source,
            })?;
        let error = output.try_clone().await.map_err(|source| Error::Log {
            path: log.to_path_buf(),
            source,
        })?;
        let output = output.into_std().await;
        let error = error.into_std().await;
        let mut command = Command::new("radicle-node");
        command
            .arg("--log-level")
            .arg("info")
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error))
            .kill_on_drop(true);
        identity.configure_radicle_command(command.as_std_mut());
        command.spawn().map_err(Error::Spawn)
    }

    /// Returns the connected Radicle node handle.
    #[must_use]
    pub const fn node(&self) -> &Node {
        &self.node
    }

    /// Connects configured seeds and waits for their protocol handshakes before fetching.
    pub(crate) async fn connect_seeds(
        &self,
        identity: &Identity,
        repository: RepoId,
        shutdown: &Shutdown,
    ) -> Result<(), Error> {
        let config = &identity.profile().config;
        let mut seeds = config.preferred_seeds.clone();
        for peer in &config.node.connect {
            if !seeds.contains(peer) {
                seeds.push(peer.clone());
            }
        }
        if seeds.is_empty() {
            return Ok(());
        }

        let connections = seeds.into_iter().map(|seed| {
            Self::connect_seed(
                self.node.clone(),
                seed,
                shutdown.clone(),
                CONNECTION_TIMEOUT,
            )
        });
        // A usable repository seed lets us proceed while other preferred seeds are unavailable.
        // Private repositories may not advertise their inventory, so connected preferred seeds
        // remain a fallback when their connection attempts finish.
        let results = tokio::select! {
            results = futures_util::future::join_all(connections) => results,
            result = self.wait_for_repository_seed(repository, *identity.profile().id()) => {
                return if shutdown.is_requested() { Ok(()) } else { result };
            }
        };
        if shutdown.is_requested() || results.iter().any(Result::is_ok) {
            return Ok(());
        }
        // A previously discovered peer can still serve the repository when preferred seeds fail.
        let node = self.node.clone();
        let sessions = tokio::task::spawn_blocking(move || node.sessions())
            .await
            .map_err(|error| Error::Task(error.to_string()))?
            .map_err(Error::Control)?;
        if sessions.iter().any(radicle::node::Session::is_connected) {
            return Ok(());
        }

        Err(Error::SeedsUnavailable {
            details: results
                .into_iter()
                .filter_map(Result::err)
                .collect::<Vec<_>>()
                .join("; "),
            log: self.log.clone(),
        })
    }

    async fn wait_for_repository_seed(&self, repository: RepoId, id: NodeId) -> Result<(), Error> {
        loop {
            let mut node = self.node.clone();
            let seeds = tokio::task::spawn_blocking(move || node.seeds_for(repository, [id]))
                .await
                .map_err(|error| Error::Task(error.to_string()))?
                .map_err(Error::Control)?;
            if seeds.connected().next().is_some() {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn connect_seed(
        node: Node,
        seed: ConnectAddress,
        shutdown: Shutdown,
        timeout: Duration,
    ) -> Result<(), String> {
        let peer = seed.to_string();
        tracing::info!(seed = %peer, "Connecting to Radicle seed");
        tokio::time::timeout(timeout, Self::connect_and_wait(node, seed, shutdown))
            .await
            .map_err(|_elapsed| format!("{peer}: connection timed out"))?
            .map_err(|error| format!("{peer}: {error}"))
    }

    async fn connect_and_wait(
        node: Node,
        seed: ConnectAddress,
        shutdown: Shutdown,
    ) -> Result<(), String> {
        let id = seed.id;
        let session = Self::seed_session(node.clone(), id).await?;
        if session.is_none_or(|session| matches!(session.state, State::Disconnected { .. })) {
            let mut connection = node.clone();
            let result = tokio::task::spawn_blocking(move || {
                connection.connect(
                    id,
                    seed.into(),
                    ConnectOptions {
                        persistent: true,
                        // Session checks below handle handshakes that outlast this request.
                        timeout: CONNECT_REQUEST_TIMEOUT,
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())?;
            match result {
                Ok(ConnectResult::Connected) | Err(radicle::node::Error::TimedOut) => {}
                Ok(ConnectResult::Disconnected { reason }) => return Err(reason),
                Err(error) => return Err(error.to_string()),
            }
        }

        // `connect` may succeed while the session is still `Attempted`. Fetching then fails with
        // "peer is not connected" or "No addresses known", even for a configured seed.
        loop {
            if shutdown.is_requested() {
                return Ok(());
            }
            let session = Self::seed_session(node.clone(), id).await?;
            if session.is_some_and(|session| session.is_connected()) {
                tracing::info!(seed = %id, "Connected to Radicle seed");
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn seed_session(node: Node, id: NodeId) -> Result<Option<Session>, String> {
        tokio::task::spawn_blocking(move || node.session(id))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
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
    pub async fn stop(self) -> Result<(), Error> {
        self.stop_supervised(&Shutdown::default()).await
    }

    pub(crate) async fn stop_supervised(mut self, shutdown: &Shutdown) -> Result<(), Error> {
        if shutdown.shutdown_started() {
            return self.reap_child(SHUTDOWN_TIMEOUT).await;
        }
        match Self::shutdown_node(self.node.clone()).await {
            Ok(()) => {
                shutdown.mark_shutdown_started();
                self.reap_child(SHUTDOWN_TIMEOUT).await
            }
            Err(_) if shutdown.shutdown_started() => self.reap_child(SHUTDOWN_TIMEOUT).await,
            Err(error) => {
                self.terminate_and_reap().await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn reap(mut self) -> Result<(), Error> {
        self.reap_child(SHUTDOWN_TIMEOUT).await
    }

    async fn terminate_and_reap(&self) -> Result<(), Error> {
        self.child.kill().map_err(Error::Wait)?;
        let _status = self.child.wait().await.map_err(Error::Wait)?;
        self.remove_owned_socket()
    }

    async fn reap_child(&mut self, timeout: Duration) -> Result<(), Error> {
        let status = match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => status.map_err(Error::Wait)?,
            Err(_elapsed) => {
                self.terminate_and_reap().await?;
                return Err(Error::ShutdownTimeout {
                    timeout,
                    log: self.log.clone(),
                });
            }
        };
        self.remove_owned_socket()?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Exited {
                status,
                log: self.log.clone(),
            })
        }
    }

    // Only called after our child has been reaped. A forced stop cannot unlink its own socket, and
    // leaving it behind would prevent the next setup attempt from starting a node.
    fn remove_owned_socket(&self) -> Result<(), Error> {
        match fs::symlink_metadata(&self.socket) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&self.socket)
                .map_err(|source| Error::Socket {
                    path: self.socket.clone().into(),
                    source,
                }),
            Ok(_) => Err(Error::SocketOccupied(self.socket.clone().into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Socket {
                path: self.socket.clone().into(),
                source,
            }),
        }
    }

    async fn wait_until_ready(&mut self, expected: NodeId, timeout: Duration) -> Result<(), Error> {
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
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            if let Ok(actual) =
                Self::node_id(self.node.clone(), remaining.min(READINESS_REQUEST_TIMEOUT)).await
            {
                if actual != expected {
                    return Err(Error::IdentityMismatch { expected, actual });
                }
                tracing::info!(node = %actual, "Radicle node ready");
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed()))).await;
        }

        Err(Error::StartupTimeout {
            timeout,
            log: self.log.clone(),
        })
    }

    async fn node_id(node: Node, timeout: Duration) -> Result<NodeId, Error> {
        tokio::task::spawn_blocking(move || {
            node.call::<NodeId>(NodeCommand::NodeId, timeout)?
                .next()
                .ok_or(radicle::node::Error::EmptyResponse)?
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
        .map_err(Error::Control)
    }

    async fn shutdown_node(node: Node) -> Result<(), Error> {
        tokio::task::spawn_blocking(move || Self::request_shutdown(&node))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
            .map_err(Error::Control)
    }

    // `Node::shutdown` polls until the node stops, with no overall deadline. Only request shutdown
    // here; the owned child is waited for separately and killed if it does not exit.
    pub(crate) fn request_shutdown(node: &Node) -> Result<(), radicle::node::Error> {
        node.call::<Success>(NodeCommand::Shutdown, CONNECT_REQUEST_TIMEOUT)?
            .next()
            .transpose()?;
        Ok(())
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
    #[error("failed to access Radicle control socket '{path}': {source}")]
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
    /// A blocking Radicle operation task failed.
    #[error("managed node task failed: {0}")]
    Task(String),
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
    /// The child did not stop after its grace period and was forcibly terminated.
    #[error("radicle-node did not stop within {timeout:?} and was terminated; see '{log}'")]
    ShutdownTimeout {
        /// Time allowed for graceful shutdown.
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
    /// No configured seed or previously discovered peer completed a connection.
    #[error(
        "could not connect to a Radicle seed: {details}. Check seed availability and network \
         access (node log: {log})"
    )]
    SeedsUnavailable {
        /// Connection failure for each configured seed.
        details: String,
        /// Log containing the node's diagnostics.
        log: PathBuf,
    },
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use radicle::node::{Node, NodeId};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use crate::{
        node::{ChildHandle, Error, NodeManager},
        shutdown::Shutdown,
    };

    const NODE: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[tokio::test]
    async fn shutdown_acknowledgment_does_not_wait_for_the_node_to_exit() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let socket = root.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("socket should bind");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("client should connect");
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream
                .read_line(&mut request)
                .await
                .expect("request should arrive");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).expect("request should parse"),
                serde_json::json!({"command": "shutdown"})
            );
            stream
                .write_all(b"{}\n")
                .await
                .expect("response should be sent");
            // Keep both the socket and node alive after acknowledging shutdown.
            std::future::pending::<()>().await;
        });

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            NodeManager::shutdown_node(Node::new(socket)),
        )
        .await;
        server.abort();
        assert!(server.await.expect_err("server should stop").is_cancelled());
        result
            .expect("shutdown acknowledgment must not wait for EOF or process exit")
            .expect("shutdown should be acknowledged");
    }

    #[tokio::test]
    async fn terminates_and_reaps_a_node_that_does_not_stop() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let socket = root.path().join("control.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).expect("socket should bind");
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .kill_on_drop(true)
            .spawn()
            .expect("test child should start");
        let handle = ChildHandle::new(child);
        let mut manager = NodeManager {
            child: handle.clone(),
            node: Node::new("unused-control.sock"),
            socket: socket.clone(),
            log: PathBuf::from("node.log"),
        };

        assert!(matches!(
            manager.reap_child(Duration::from_millis(50)).await,
            Err(Error::ShutdownTimeout { .. })
        ));
        assert_eq!(
            handle
                .wait()
                .await
                .expect_err("child must have been reaped")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(
            !socket.exists(),
            "the next setup must not inherit an abandoned socket"
        );
    }

    #[tokio::test]
    async fn accepts_a_node_that_stops_within_its_deadline() {
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("test child should start");
        let mut manager = NodeManager {
            child: ChildHandle::new(child),
            node: Node::new("unused-control.sock"),
            socket: PathBuf::from("unused-control.sock"),
            log: PathBuf::from("node.log"),
        };
        manager
            .reap_child(Duration::from_secs(1))
            .await
            .expect("node should stop normally");
    }

    #[tokio::test]
    async fn reaps_the_child_when_the_shutdown_socket_is_unavailable() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let socket = root.path().join("missing.sock");
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .kill_on_drop(true)
            .spawn()
            .expect("test child should start");
        let handle = ChildHandle::new(child);
        let manager = NodeManager {
            child: handle.clone(),
            node: Node::new(&socket),
            socket,
            log: root.path().join("node.log"),
        };

        assert!(matches!(manager.stop().await, Err(Error::Control(_))));
        assert_eq!(
            handle
                .wait()
                .await
                .expect_err("failed shutdown must still reap the child")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn a_repository_seed_does_not_wait_for_an_unavailable_preferred_seed() {
        use crate::{
            home::MemberHome,
            identity::{KeyProtection, Manager, Provider},
            member::Member,
        };

        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
            .parse()
            .expect("RID should parse");
        let home = MemberHome::at(root.path());
        home.record(Member::Nico, repository)
            .expect("home should be created");
        let identity = Manager::default()
            .load(
                Member::Nico,
                &home,
                &Provider::Managed {
                    protection: KeyProtection::Unencrypted,
                },
            )
            .expect("identity should be created");
        assert!(identity.profile().config.preferred_seeds.len() > 1);

        let socket = root.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("socket should bind");
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("client should connect");
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream
                    .read_line(&mut request)
                    .await
                    .expect("request should arrive");
                let request: serde_json::Value =
                    serde_json::from_str(&request).expect("request should be JSON");
                let response = if request.get("command").and_then(serde_json::Value::as_str)
                    == Some("seedsFor")
                {
                    serde_json::json!([{
                        "nid": NODE, "addrs": [], "state": {"connected": {"since": 0}},
                    }])
                } else {
                    let id = request
                        .get("nid")
                        .and_then(serde_json::Value::as_str)
                        .expect("request should query a session");
                    let state = if id == NODE {
                        serde_json::json!({"connected": {"since": 0}})
                    } else {
                        serde_json::json!("attempted")
                    };
                    serde_json::json!({
                        "nid": id, "link": "outbound", "addr": "127.0.0.1:8776", "state": state,
                    })
                };
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("response should be sent");
            }
        });
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .expect("test child should start");
        let manager = NodeManager {
            child: ChildHandle::new(child),
            node: Node::new(&socket),
            socket,
            log: home.node_log(),
        };
        let connected = tokio::time::timeout(
            Duration::from_secs(1),
            manager.connect_seeds(&identity, repository, &Shutdown::default()),
        )
        .await;
        manager
            .terminate_and_reap()
            .await
            .expect("test child should stop");
        server.abort();
        assert!(server.await.expect_err("server should stop").is_cancelled());
        connected
            .expect("an offline seed must not delay a usable repository seed")
            .expect("the repository seed should be ready");
    }

    // A successful connect response can precede the actual protocol handshake.
    async fn seed_control(
        connected_after: Option<usize>,
    ) -> (tempfile::TempDir, Node, tokio::task::JoinHandle<usize>) {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let socket = root.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("control socket should bind");
        let server = tokio::spawn(async move {
            let mut polls = 0;
            let mut connecting = false;
            loop {
                let (stream, _) = listener.accept().await.expect("client should connect");
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream
                    .read_line(&mut request)
                    .await
                    .expect("request should arrive");
                let request: serde_json::Value =
                    serde_json::from_str(&request).expect("request should be JSON");
                let response = if request.get("command").and_then(serde_json::Value::as_str)
                    == Some("connect")
                {
                    connecting = true;
                    serde_json::json!({"status": "connected"})
                } else if !connecting {
                    serde_json::Value::Null
                } else {
                    polls += 1;
                    let state = if connected_after.is_some_and(|count| polls >= count) {
                        serde_json::json!({"connected": {"since": 0}})
                    } else {
                        serde_json::json!("attempted")
                    };
                    serde_json::json!({
                        "nid": NODE,
                        "link": "outbound",
                        "addr": "127.0.0.1:8776",
                        "state": state,
                    })
                };
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("response should be sent");
                if connected_after.is_some_and(|count| polls >= count) {
                    return polls;
                }
            }
        });
        (root, Node::new(socket), server)
    }

    #[tokio::test]
    async fn waits_for_a_seed_handshake_after_connect_succeeds() {
        let (_root, node, server) = seed_control(Some(3)).await;
        let seed = serde_json::from_value(serde_json::json!(format!("{NODE}@127.0.0.1:8776")))
            .expect("seed should parse");

        NodeManager::connect_seed(node, seed, Shutdown::default(), Duration::from_secs(1))
            .await
            .expect("seed should finish connecting");

        let polls = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("handshake should be observed")
            .expect("control server should complete");
        assert_eq!(polls, 3, "an attempted connection must not permit a fetch");
    }

    #[tokio::test]
    async fn reports_a_seed_that_never_finishes_connecting() {
        let (_root, node, server) = seed_control(None).await;
        let seed = serde_json::from_value(serde_json::json!(format!("{NODE}@127.0.0.1:8776")))
            .expect("seed should parse");

        let error =
            NodeManager::connect_seed(node, seed, Shutdown::default(), Duration::from_millis(150))
                .await
                .expect_err("a pending connection must time out");

        assert!(error.contains(NODE));
        assert!(error.contains("connection timed out"));
        server.abort();
        assert!(server.await.expect_err("server should stop").is_cancelled());
    }

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

    #[tokio::test]
    async fn startup_waits_for_a_delayed_response_and_bounds_an_unresponsive_socket() {
        for responds in [true, false] {
            let root = tempfile::tempdir().expect("temporary member home");
            let socket = root.path().join("control.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("control socket");
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("readiness connection");
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream
                    .read_line(&mut request)
                    .await
                    .expect("readiness request");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&request).expect("request JSON"),
                    serde_json::json!({"command": "nodeId"})
                );
                if responds {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    stream
                        .write_all(format!("\"{NODE}\"\n").as_bytes())
                        .await
                        .expect("delayed node identity");
                } else {
                    std::future::pending::<()>().await;
                }
            });
            let child = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exec sleep 60"])
                .kill_on_drop(true)
                .spawn()
                .expect("managed child");
            let mut manager = NodeManager {
                child: ChildHandle::new(child),
                node: Node::new(&socket),
                socket,
                log: root.path().join("node.log"),
            };
            let expected = NODE.parse::<NodeId>().expect("expected node identity");
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                manager.wait_until_ready(expected, Duration::from_millis(300)),
            )
            .await;
            if responds {
                server.await.expect("control response task");
            } else {
                server.abort();
                assert!(
                    server
                        .await
                        .expect_err("stalled server stopped")
                        .is_cancelled()
                );
            }
            manager.terminate_and_reap().await.expect("startup cleanup");
            let result = result.expect("a control probe must respect the startup deadline");
            if responds {
                result.expect("wait for a node that becomes responsive during startup");
            } else {
                assert!(matches!(result, Err(Error::StartupTimeout { .. })));
            }
        }
    }

    #[tokio::test]
    async fn reports_a_child_that_exits_during_startup() {
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("test child should start");
        let socket = std::env::temp_dir().join(format!(
            "radicle-team-missing-socket-{}",
            std::process::id()
        ));
        let mut manager = NodeManager {
            child: ChildHandle::new(child),
            node: Node::new(&socket),
            socket,
            log: PathBuf::from("node.log"),
        };
        let expected = NODE.parse::<NodeId>().expect("node ID should parse");

        assert!(matches!(
            manager
                .wait_until_ready(expected, Duration::from_secs(1))
                .await,
            Err(Error::Exited { .. })
        ));
    }

    #[tokio::test]
    async fn forced_startup_cleanup_reaps_the_child() {
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .spawn()
            .expect("test child should start");
        let handle = ChildHandle::new(child);
        let manager = NodeManager {
            child: handle.clone(),
            node: Node::new(std::env::temp_dir().join("radicle-team-unused-socket")),
            socket: std::env::temp_dir().join("radicle-team-unused-socket"),
            log: PathBuf::from("node.log"),
        };

        manager
            .terminate_and_reap()
            .await
            .expect("child should be terminated and reaped");

        assert_eq!(
            handle
                .wait()
                .await
                .expect_err("reaped child should no longer be owned")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
