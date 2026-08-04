//! Serialized delivery to the chatting adapter of a member.

use radicle::identity::RepoId;
use tokio::sync::{mpsc, oneshot};

use crate::{
    agent::{ChatContext, ChatMessage, Chatter, Source},
    chat::{MessageId, classification::IncomingMessage},
    conversation::{Acknowledgement, Error, Scoped},
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
    ) -> Result<Acknowledgement, Error> {
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
                scoped: Scoped::new(member, workspaces, scope),
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
    ) -> Result<Acknowledgement, Error> {
        let source = Source::Chat(message.channel.clone());
        let (workspace, path) = self.scoped.prepare(identity.into(), source).await?;
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
        let handled = agent
            .messaged(
                context,
                ChatMessage {
                    nick: message.nick,
                    body: message.body,
                },
            )
            .await;
        self.scoped
            .finish(identity.into(), workspace, handled)
            .await
    }
}

struct Request {
    repository: RepoId,
    identity: MessageId,
    message: IncomingMessage,
    completion: oneshot::Sender<Result<Acknowledgement, Error>>,
}
