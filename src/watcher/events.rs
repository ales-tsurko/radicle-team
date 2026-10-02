//! Continuously drains node events and coalesces repository rescan requests.

use std::{io::ErrorKind, time::Duration};

use radicle::{
    cob::TypedId,
    identity::RepoId,
    node::{Handle as _, LineIter, Node, events::Event},
    storage::RefUpdate,
};
use tokio::sync::{oneshot, watch};

use crate::watcher::Error;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_millis(250);

pub(super) struct NodeEvents {
    pub(super) changes: watch::Receiver<()>,
    pub(super) failure: oneshot::Receiver<Error>,
}

impl NodeEvents {
    pub(super) async fn subscribe(repository: RepoId, node: Node) -> Result<Self, Error> {
        let (updates, changes) = watch::channel(());
        let (failed, failure) = oneshot::channel();
        let (ready, readiness) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let events = match node.subscribe(READ_TIMEOUT) {
                Ok(events) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    events
                }
                Err(error) => {
                    let _sent = ready.send(Err(Error::from(error)));
                    return;
                }
            };
            if let Err(error) = Self::consume(repository, node, events, &updates) {
                let _sent = failed.send(error);
            }
        });
        readiness
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        Ok(Self { changes, failure })
    }

    fn consume(
        repository: RepoId,
        node: Node,
        mut events: LineIter<Event>,
        updates: &watch::Sender<()>,
    ) -> Result<(), Error> {
        while !updates.is_closed() {
            match events.next() {
                Some(Ok(event)) => {
                    if Self::updates_repository(repository, event) && updates.send(()).is_err() {
                        return Ok(());
                    }
                }
                Some(Err(radicle::node::Error::TimedOut)) => {}
                ended if Self::can_reconnect(&ended) => {
                    tracing::warn!(%repository, "Radicle event subscription ended; reconnecting");
                    std::thread::sleep(RECONNECT_DELAY);
                    if updates.is_closed() {
                        return Ok(());
                    }
                    // A closed subscription does not mean the node died. An unavailable node
                    // still fails here, rather than leaving the member running without ingress.
                    node.nid()?;
                    events = node.subscribe(READ_TIMEOUT)?;
                    if updates.send(()).is_err() {
                        return Ok(());
                    }
                    tracing::info!(%repository, "Radicle event subscription restored; rescanning");
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Err(Error::EventStreamClosed),
            }
        }
        Ok(())
    }

    fn can_reconnect(event: &Option<Result<Event, radicle::node::Error>>) -> bool {
        match event {
            None => true,
            Some(Err(radicle::node::Error::Io(error))) => matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
            ),
            Some(_) => false,
        }
    }

    fn updates_repository(repository: RepoId, event: Event) -> bool {
        matches!(event, Event::RefsFetched { rid, updated, .. }
            if rid == repository && Self::updates_cobs(&updated))
    }

    fn updates_cobs(updates: &[RefUpdate]) -> bool {
        updates.iter().any(|update| {
            update
                .name()
                .to_namespaced()
                .and_then(|name| TypedId::from_namespaced(&name).ok().flatten())
                .is_some_and(|id| id.is_issue() || id.is_patch())
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::{BufRead as _, BufReader},
        os::unix::net::{UnixListener, UnixStream},
        str::FromStr as _,
        sync::mpsc,
        time::Duration,
    };

    use radicle::{
        crypto::PublicKey,
        identity::RepoId,
        node::{Command, CommandResult, Node, events::Event},
        storage::RefUpdate,
    };

    use crate::watcher::{Error, events::NodeEvents};

    const KEY: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const OBJECT: &str = "1111111111111111111111111111111111111111";
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct SocketNode {
        root: tempfile::TempDir,
        listener: UnixListener,
    }

    impl SocketNode {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let listener = UnixListener::bind(root.path().join("control.sock"))
                .expect("control socket should bind");
            Self { root, listener }
        }

        fn node(&self) -> Node {
            Node::new(self.root.path().join("control.sock"))
        }

        fn accept(&self) -> (UnixStream, Command) {
            let (stream, _) = self.listener.accept().expect("command should connect");
            stream
                .set_read_timeout(Some(TEST_TIMEOUT))
                .expect("read should be bounded");
            stream
                .set_write_timeout(Some(TEST_TIMEOUT))
                .expect("write should be bounded");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("command should be read");
            let command = serde_json::from_str(&line).expect("command should be valid");
            (stream, command)
        }

        fn reply<T: serde::Serialize>(stream: &mut UnixStream, value: T) {
            CommandResult::Okay(value)
                .to_writer(stream)
                .expect("response should be written");
        }

        fn fetched() -> Event {
            Event::RefsFetched {
                remote: PublicKey::from_str(KEY).expect("key should be valid"),
                rid: RepoId::from_str(REPOSITORY).expect("repository should be valid"),
                updated: vec![RefUpdate::Created {
                    name: format!("refs/namespaces/{KEY}/refs/cobs/xyz.radicle.issue/{OBJECT}")
                        .try_into()
                        .expect("reference should be valid"),
                    oid: OBJECT.parse().expect("object should be valid"),
                }],
            }
        }
    }

    #[tokio::test]
    async fn drains_events_while_both_scanners_are_busy() {
        let fixture = SocketNode::new();
        let node = fixture.node();
        let (written, received) = tokio::sync::oneshot::channel();
        let (release, released) = mpsc::channel();
        let server = tokio::task::spawn_blocking(move || {
            let (mut stream, command) = fixture.accept();
            assert!(matches!(command, Command::Subscribe));
            let event = SocketNode::fetched();
            for _ in 0..10_000 {
                SocketNode::reply(&mut stream, &event);
            }
            written.send(()).expect("writes should be reported");
            released
                .recv_timeout(TEST_TIMEOUT)
                .expect("server should be released");
        });
        let repository = REPOSITORY.parse().expect("repository should be valid");
        let mut events = NodeEvents::subscribe(repository, node)
            .await
            .expect("events should open");
        let mut assignments = events.changes.clone();

        tokio::time::timeout(TEST_TIMEOUT, received)
            .await
            .expect("busy scanners must not block node writes")
            .expect("server should finish writing");
        assert!(
            events
                .changes
                .has_changed()
                .expect("conversation scanner should be notified")
        );
        assert!(
            assignments
                .has_changed()
                .expect("assignment scanner should be notified")
        );
        events.changes.borrow_and_update();
        assignments.borrow_and_update();
        assert!(matches!(
            events.failure.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(assignments);
        drop(events);
        release.send(()).expect("server should be released");
        server.await.expect("server should finish");
    }

    #[tokio::test]
    async fn reconnects_and_requests_a_rescan_without_another_fetch() {
        let fixture = SocketNode::new();
        let node = fixture.node();
        let (release, released) = mpsc::channel();
        let server = tokio::task::spawn_blocking(move || {
            let (stream, command) = fixture.accept();
            assert!(matches!(command, Command::Subscribe));
            drop(stream);
            let (mut stream, command) = fixture.accept();
            assert!(matches!(command, Command::NodeId));
            SocketNode::reply(
                &mut stream,
                PublicKey::from_str(KEY).expect("key should be valid"),
            );
            drop(stream);
            let (_stream, command) = fixture.accept();
            assert!(matches!(command, Command::Subscribe));
            released
                .recv_timeout(TEST_TIMEOUT)
                .expect("server should be released");
        });
        let repository = REPOSITORY.parse().expect("repository should be valid");
        let mut events = NodeEvents::subscribe(repository, node)
            .await
            .expect("events should open");
        tokio::time::timeout(TEST_TIMEOUT, events.changes.changed())
            .await
            .expect("reconnection should request a rescan")
            .expect("subscription should remain open");
        assert!(matches!(
            events.failure.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(events);
        release.send(()).expect("server should be released");
        server.await.expect("server should finish");
    }

    #[tokio::test]
    async fn reports_a_node_that_really_disappeared() {
        let fixture = SocketNode::new();
        let node = fixture.node();
        let server = tokio::task::spawn_blocking(move || {
            let (stream, command) = fixture.accept();
            assert!(matches!(command, Command::Subscribe));
            drop(stream);
        });
        let repository = REPOSITORY.parse().expect("repository should be valid");
        let events = NodeEvents::subscribe(repository, node)
            .await
            .expect("events should open");
        let failure = tokio::time::timeout(TEST_TIMEOUT, events.failure)
            .await
            .expect("node disappearance should be reported")
            .expect("failure should be sent");
        assert!(matches!(
            failure,
            Error::Node(radicle::node::Error::Connect(..))
        ));
        drop(events.changes);
        server.await.expect("server should finish");
    }
}
