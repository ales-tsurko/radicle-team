//! Cooperative process shutdown.
//!
//! The CLI installs one handler for interruption and termination signals. The handler records the
//! shutdown request and asks the currently supervised Radicle node to stop through its control
//! socket. Application code then returns normally, allowing [`NodeManager`] to reap the child. If
//! no node can accept the request, the handler terminates the CLI instead of suppressing the
//! operating-system signal indefinitely. This is necessary because operating-system termination
//! does not run Rust destructors by itself.

use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};

use radicle::node::Node;

use crate::node::{ChildHandle, NodeManager};

/// Coordinates one process-wide shutdown request with its managed node.
#[derive(Debug, Clone, Default)]
pub(crate) struct Shutdown {
    state: Arc<State>,
}

impl Shutdown {
    /// Installs the CLI signal handler.
    pub(crate) fn install() -> Result<Self, ctrlc::Error> {
        let shutdown = Self {
            state: Arc::new(State {
                runtime: Some(tokio::runtime::Handle::current()),
                ..State::default()
            }),
        };
        let handler = shutdown.clone();
        ctrlc::set_handler(move || {
            if !handler.request() {
                std::process::exit(1);
            }
        })?;

        Ok(shutdown)
    }

    /// Returns whether process shutdown was requested.
    pub(crate) fn is_requested(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }

    /// Returns whether managed-node shutdown has started.
    pub(crate) fn shutdown_started(&self) -> bool {
        self.state.shutdown_started.load(Ordering::Acquire)
    }

    /// Records a graceful shutdown initiated by application cleanup.
    pub(crate) fn mark_shutdown_started(&self) {
        self.state.shutdown_started.store(true, Ordering::Release);
    }

    /// Defers process exit while a child is being spawned for supervision.
    pub(crate) fn begin_start(&self) {
        self.state.shutdown_started.store(false, Ordering::Release);
        self.state.starting.store(true, Ordering::Release);
    }

    /// Ends a failed start that produced no child.
    pub(crate) fn cancel_start(&self) {
        self.state.starting.store(false, Ordering::Release);
    }

    /// Associates a running node with this process.
    pub(crate) fn supervise(&self, node: &Node, child: ChildHandle) {
        *self.node() = Some(SupervisedNode {
            handle: node.clone(),
            child,
        });
        self.state.starting.store(false, Ordering::Release);
        if self.is_requested() {
            let _scheduled = self.schedule_shutdown();
        }
    }

    pub(crate) fn release_supervision(&self) {
        let _node = self.node().take();
    }

    /// Returns whether the signal was handled without immediate process termination.
    fn request(&self) -> bool {
        let repeated = self.state.requested.swap(true, Ordering::AcqRel);
        if repeated || self.shutdown_started() {
            return match self.kill_child() {
                RequestResult::Missing if self.state.starting.load(Ordering::Acquire) => true,
                RequestResult::Missing | RequestResult::Failed => false,
                RequestResult::Handled => false,
            };
        }
        match self.schedule_shutdown() {
            RequestResult::Handled => true,
            RequestResult::Failed => false,
            RequestResult::Missing if self.state.starting.load(Ordering::Acquire) => true,
            RequestResult::Missing => {
                matches!(self.schedule_shutdown(), RequestResult::Handled)
            }
        }
    }

    fn kill_child(&self) -> RequestResult {
        let Some(node) = self.supervised_node() else {
            return RequestResult::Missing;
        };
        if node.child.kill().is_ok() {
            RequestResult::Handled
        } else {
            RequestResult::Failed
        }
    }

    fn schedule_shutdown(&self) -> RequestResult {
        let Some(node) = self.supervised_node() else {
            return RequestResult::Missing;
        };
        if self.state.shutdown_started.swap(true, Ordering::AcqRel) {
            return RequestResult::Handled;
        }
        let child = node.child.clone();
        let Some(runtime) = self.state.runtime.clone() else {
            let _result = child.kill();
            self.state.shutdown_started.store(false, Ordering::Release);
            return RequestResult::Failed;
        };
        runtime.spawn_blocking(move || {
            if NodeManager::request_shutdown(&node.handle).is_err() {
                let _result = child.kill();
            }
        });
        RequestResult::Handled
    }

    fn supervised_node(&self) -> Option<SupervisedNode> {
        self.node().clone()
    }

    fn node(&self) -> MutexGuard<'_, Option<SupervisedNode>> {
        self.state
            .node
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Default)]
struct State {
    requested: AtomicBool,
    shutdown_started: AtomicBool,
    starting: AtomicBool,
    node: Mutex<Option<SupervisedNode>>,
    runtime: Option<tokio::runtime::Handle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestResult {
    Handled,
    Failed,
    Missing,
}

#[derive(Debug, Clone)]
struct SupervisedNode {
    handle: Node,
    child: ChildHandle,
}

#[cfg(test)]
mod tests {
    use crate::shutdown::Shutdown;

    #[test]
    fn remembers_a_shutdown_request() {
        let shutdown = Shutdown::default();
        assert!(!shutdown.request());

        assert!(shutdown.is_requested());
    }

    #[test]
    fn defers_exit_while_a_node_is_starting() {
        let shutdown = Shutdown::default();
        shutdown.begin_start();

        assert!(shutdown.request());
        shutdown.cancel_start();
    }

    #[test]
    fn resets_shutdown_state_for_a_replacement_node() {
        let shutdown = Shutdown::default();
        shutdown.mark_shutdown_started();

        shutdown.begin_start();

        assert!(!shutdown.shutdown_started());
    }

    #[cfg(all(unix, not(miri)))]
    fn supervised_test_child() -> (Shutdown, crate::node::ChildHandle) {
        use radicle::node::Node;

        use crate::node::ChildHandle;

        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .expect("test child should start");
        let node = Node::new(std::env::temp_dir().join(format!(
            "radicle-team-unresponsive-node-{}",
            child.id().expect("test child should have an ID")
        )));
        let handle = ChildHandle::new(child);
        let shutdown = Shutdown {
            state: std::sync::Arc::new(super::State {
                runtime: Some(tokio::runtime::Handle::current()),
                ..super::State::default()
            }),
        };
        shutdown.supervise(&node, handle.clone());

        (shutdown, handle)
    }

    #[cfg(all(unix, not(miri)))]
    #[tokio::test]
    async fn kills_a_child_when_control_shutdown_fails() {
        let (shutdown, handle) = supervised_test_child();

        assert!(shutdown.request());
        assert!(
            !handle
                .wait()
                .await
                .expect("test child should be reaped")
                .success()
        );
    }

    #[cfg(all(unix, not(miri)))]
    #[tokio::test]
    async fn a_signal_forces_a_child_that_acknowledged_shutdown() {
        let (shutdown, handle) = supervised_test_child();
        shutdown.mark_shutdown_started();

        assert!(!shutdown.request());
        assert!(
            !handle
                .wait()
                .await
                .expect("test child should be reaped")
                .success()
        );
    }
}
