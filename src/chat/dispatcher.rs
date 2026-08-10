//! Serialized delivery to the chatting adapter of a member.

use radicle::identity::RepoId;
use tokio::sync::{mpsc, oneshot};

use crate::{
    agent::{Adapter, ChatContext, ChatMessage, Chatter, Source},
    chat::{MessageId, classification::IncomingMessage},
    conversation::{Error, Scoped},
    mcp::WorkScope,
    member::Member,
    workspace::Workspaces,
};

const BUFFER: usize = 32;

/// Sends authenticated chat to the dispatcher which owns the chatting adapter.
#[derive(Debug, Clone)]
pub(crate) struct Handle {
    sender: mpsc::Sender<Request>,
}

impl Handle {
    /// Delivers one authenticated message and waits until handling and publication recovery finish.
    pub(crate) async fn message(
        &self,
        repository: RepoId,
        identity: MessageId,
        message: IncomingMessage,
    ) -> Result<(), Error> {
        let (completion, completed) = oneshot::channel();
        self.sender
            .send(Request {
                repository,
                identity,
                message,
                completion,
            })
            .await
            .map_err(|_error| Error::Unavailable)?;
        completed.await.map_err(|_error| Error::Unavailable)?
    }
}

// Owns the chatting adapter's workspace and MCP scope while serializing messages.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    scoped: Scoped,
    receiver: mpsc::Receiver<Request>,
}

impl Dispatcher {
    // Creates the dispatcher and its clonable ingress handle.
    pub(crate) fn new(member: Member, workspaces: Workspaces, scope: WorkScope) -> (Handle, Self) {
        let (sender, receiver) = mpsc::channel(BUFFER);
        (
            Handle { sender },
            Self {
                scoped: Scoped::new(member, Adapter::Chatter, workspaces, scope),
                receiver,
            },
        )
    }

    // Runs until every ingress handle has been dropped.
    pub(crate) async fn run(mut self, agent: &mut (impl Chatter + ?Sized)) -> Result<(), Error> {
        while let Some(request) = self.receiver.recv().await {
            let Request {
                repository,
                identity,
                message,
                completion,
            } = request;
            let result = self.dispatch(repository, identity, message, agent).await;
            let _delivered = completion.send(result);
        }
        Err(Error::Unavailable)
    }

    async fn dispatch(
        &self,
        repository: RepoId,
        identity: MessageId,
        message: IncomingMessage,
        agent: &mut (impl Chatter + ?Sized),
    ) -> Result<(), Error> {
        let source = Source::Chat(message.channel.clone());
        let path = self.scoped.prepare(source).await?;
        let context =
            ChatContext::authenticated(repository, message.channel, identity, message.sender, path);
        self.scoped
            .scope
            .activate(context.clone())
            .await
            .map_err(Error::WorkScope)?;
        tracing::info!(
            member = %self.scoped.member,
            %repository,
            channel = %context.channel(),
            message = %identity,
            "dispatching chat message to the agent"
        );
        let watch = crate::agent::Supervised::supervisor(agent);
        let handled = crate::conversation::supervise(
            agent.messaged(
                context,
                ChatMessage {
                    nick: message.nick,
                    body: message.body,
                },
            ),
            watch,
        )
        .await;
        self.scoped.finish(identity.into(), handled).await
    }
}

struct Request {
    repository: RepoId,
    identity: MessageId,
    message: IncomingMessage,
    completion: oneshot::Sender<Result<(), Error>>,
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{path::PathBuf, str::FromStr as _, time::Duration};

    use radicle::{
        crypto::PublicKey, git::UserInfo, identity::RepoId, node::Alias, storage::git::Storage,
    };

    use crate::{
        agent::{ChatContext, ChatMessage, Chatter, Error as AgentError, McpEndpoint},
        chat::dispatcher::Dispatcher,
        mcp::WorkScope,
        member::{Member, Profile},
        workspace::Workspaces,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const MEMBER: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";

    #[derive(Debug, Default)]
    struct SilentChatter;

    impl crate::agent::Supervised for SilentChatter {
        fn supervisor(&self) -> std::sync::Arc<dyn crate::agent::Supervisor> {
            std::sync::Arc::new(crate::agent::Idle)
        }
    }

    #[async_trait::async_trait]
    impl Chatter for SilentChatter {
        async fn start(
            &mut self,
            _profile: Profile,
            _mcp: &McpEndpoint,
            _sandbox: crate::sandbox::AgentSandbox,
            _layout: crate::agent::Layout,
        ) -> Result<(), AgentError> {
            Ok(())
        }

        async fn messaged(
            &mut self,
            _context: ChatContext,
            _message: ChatMessage,
        ) -> Result<(), AgentError> {
            Ok(())
        }
    }

    // A member whose manifest has no chat section still creates this dispatcher, and the run
    // holds its handle for the whole run. Dropping that handle instead would end the dispatcher
    // at once and take the member down with it before it had answered anything.
    #[tokio::test]
    async fn runs_for_as_long_as_a_handle_is_held() {
        let (handle, dispatcher) = Dispatcher::new(
            Member::Nico,
            workspaces("chat-dispatcher-held").await,
            WorkScope::default(),
        );
        let mut chatter = SilentChatter;

        let ended =
            tokio::time::timeout(Duration::from_millis(200), dispatcher.run(&mut chatter)).await;

        assert!(
            ended.is_err(),
            "the dispatcher must keep running while somebody can still send it a message"
        );
        drop(handle);
    }

    #[tokio::test]
    async fn ends_once_every_handle_is_gone() {
        let (handle, dispatcher) = Dispatcher::new(
            Member::Nico,
            workspaces("chat-dispatcher-gone").await,
            WorkScope::default(),
        );
        let mut chatter = SilentChatter;
        drop(handle);

        let ended = tokio::time::timeout(Duration::from_millis(200), dispatcher.run(&mut chatter))
            .await
            .expect("the dispatcher must end once nothing can reach it");

        assert!(matches!(
            ended,
            Err(crate::conversation::Error::Unavailable)
        ));
    }

    // Each test gets its own directory, because the harness runs them at the same time and one
    // clearing the other's would fail whichever lost the race.
    async fn workspaces(name: &str) -> Workspaces {
        let repository = RepoId::from_str(REPOSITORY).expect("repository should parse");
        let queue = crate::queue::WorkQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let info = UserInfo {
            alias: Alias::new("chat-test"),
            key: PublicKey::from_str(MEMBER).expect("member should be valid"),
        };
        let storage = Storage::open(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(name)
                .join("storage"),
            info.clone(),
        )
        .expect("empty storage should open");

        Workspaces::memory(name, storage, repository, info, queue)
            .expect("working copies should be created")
    }
}
