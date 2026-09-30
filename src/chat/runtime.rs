//! Reconnecting IRC ingress, durable admission, and reply delivery.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use irc_proto::Message;
use radicle::{
    identity::{Did, RepoId},
    storage::{ReadRepository as _, ReadStorage as _, git::Storage},
};

use crate::{
    chat::{
        Config, MessageId, State,
        classification::{Classification, Classifier, IncomingMessage},
        dispatcher,
        state::{MessageStatus, Receipt},
        transport::{ClientIdentity, Connection},
    },
    manifest::RepositoryPolicy,
    member::Member,
};

const RECONNECT_INITIAL: Duration = Duration::from_millis(250);
const RECONNECT_MAXIMUM: Duration = Duration::from_secs(30);
const RECONNECT_STABLE: Duration = Duration::from_secs(30);

/// Owns one member's IRC session and reconnects after transient network failures.
#[derive(Debug)]
pub(crate) struct Ingress<D = dispatcher::Handle> {
    repository: RepoId,
    member: Member,
    did: Did,
    policy: RepositoryPolicy,
    delegates: DelegateSource,
    identity: ClientIdentity,
    state: State,
    delivery: D,
}

impl<D> Ingress<D> {
    /// Creates ingress from validated runtime capabilities.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is an independently validated runtime capability"
    )]
    pub(crate) fn new(
        repository: RepoId,
        member: Member,
        did: Did,
        policy: RepositoryPolicy,
        storage: Storage,
        identity: ClientIdentity,
        state: State,
        delivery: D,
    ) -> Self {
        Self::with_delegate_source(
            repository,
            member,
            did,
            policy,
            DelegateSource::Repository(storage),
            identity,
            state,
            delivery,
        )
    }

    #[cfg(test)]
    #[expect(
        clippy::too_many_arguments,
        reason = "the test constructor mirrors independently validated runtime capabilities"
    )]
    pub(crate) fn for_test(
        repository: RepoId,
        member: Member,
        did: Did,
        policy: RepositoryPolicy,
        delegates: Vec<Did>,
        identity: ClientIdentity,
        state: State,
        delivery: D,
    ) -> Self {
        Self::with_delegate_source(
            repository,
            member,
            did,
            policy,
            DelegateSource::Fixed(delegates),
            identity,
            state,
            delivery,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is an independently validated runtime capability"
    )]
    fn with_delegate_source(
        repository: RepoId,
        member: Member,
        did: Did,
        policy: RepositoryPolicy,
        delegates: DelegateSource,
        identity: ClientIdentity,
        state: State,
        delivery: D,
    ) -> Self {
        Self {
            repository,
            member,
            did,
            policy,
            delegates,
            identity,
            state,
            delivery,
        }
    }
}

impl<D> Ingress<D>
where
    D: Delivery,
{
    /// Runs until a permanent configuration/authentication error or delivery failure occurs.
    pub(crate) async fn run(&self) -> Result<(), Error> {
        let mut delay = RECONNECT_INITIAL;
        loop {
            let failure = match self.session().await {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            };
            if !matches!(&failure.error, Error::Transport(error) if error.is_retryable()) {
                return Err(failure.error);
            }
            let retry_in = next_reconnect_delay(&mut delay, failure.connected_for);
            tracing::warn!(error = %failure.error, ?retry_in, "IRC connection interrupted");
            tokio::time::sleep(retry_in).await;
        }
    }

    async fn session(&self) -> Result<(), SessionFailure> {
        let config = self.config().map_err(SessionFailure::before_connection)?;
        let (mut connection, mut classifier) = self
            .connect(config)
            .await
            .map_err(SessionFailure::before_connection)?;
        let connected = Instant::now();
        let pending = self
            .replay(config, &mut connection, &mut classifier)
            .await
            .map_err(|error| SessionFailure::connected(error, connected))?;
        self.drain_outbox(&mut connection, &mut classifier)
            .await
            .map_err(|error| SessionFailure::connected(error, connected))?;
        self.receive(&mut connection, &mut classifier, pending)
            .await
            .map_err(|error| SessionFailure::connected(error, connected))
    }

    async fn connect(&self, config: &Config) -> Result<(Connection, Classifier), Error> {
        let connection =
            Connection::connect(config, self.member, self.did, self.identity.clone()).await?;
        let delegates = self.delegates.resolve(self.repository).await?;
        let classifier = Classifier::from_policy(
            self.repository,
            self.member,
            self.did,
            &self.policy,
            connection.server(),
            delegates,
        )?;
        Ok((connection, classifier))
    }

    async fn receive(
        &self,
        connection: &mut Connection,
        classifier: &mut Classifier,
        mut pending: VecDeque<PendingDelivery>,
    ) -> Result<(), Error> {
        loop {
            if let Some(delivery) = pending.pop_front() {
                self.deliver_while_receiving(connection, classifier, &mut pending, delivery)
                    .await?;
            } else {
                let message = connection.next().await?;
                if let Some(delivery) = self.admit(classifier, message, Source::Live).await? {
                    pending.push_back(delivery);
                }
            }
        }
    }

    async fn replay(
        &self,
        config: &Config,
        connection: &mut Connection,
        classifier: &mut Classifier,
    ) -> Result<VecDeque<PendingDelivery>, Error> {
        if !connection.has_history() {
            return Ok(VecDeque::new());
        }
        let mut pending = VecDeque::new();
        for (_logical, channel) in config.channels() {
            self.replay_channel(connection, classifier, channel, &mut pending)
                .await?;
        }
        Ok(pending)
    }

    async fn replay_channel(
        &self,
        connection: &mut Connection,
        classifier: &mut Classifier,
        channel: &crate::chat::IrcChannel,
        pending: &mut VecDeque<PendingDelivery>,
    ) -> Result<(), Error> {
        match connection.history(channel).await {
            Ok(history) => self.replay_messages(classifier, history, pending).await,
            Err(crate::chat::transport::Error::History(error)) => {
                tracing::warn!(%channel, %error, "IRC history replay unavailable");
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn replay_messages(
        &self,
        classifier: &mut Classifier,
        history: Vec<Message>,
        pending: &mut VecDeque<PendingDelivery>,
    ) -> Result<(), Error> {
        for message in history {
            if let Some(delivery) = self.admit(classifier, message, Source::History).await? {
                pending.push_back(delivery);
            }
        }
        Ok(())
    }

    async fn admit(
        &self,
        classifier: &mut Classifier,
        message: Message,
        source: Source,
    ) -> Result<Option<PendingDelivery>, Error> {
        if source == Source::Live {
            classifier.update_delegates(self.delegates.resolve(self.repository).await?);
        }
        let Classification::Relevant(message) = classifier.classify(&message) else {
            return Ok(None);
        };
        if source == Source::History && message.network_id.is_none() {
            return Ok(None);
        }
        let receipt = match source {
            Source::Live => Receipt::Live {
                message: message.network_id.as_deref(),
            },
            Source::History => Receipt::History {
                message: message
                    .network_id
                    .as_deref()
                    .expect("history without an ID was rejected above"),
            },
        };
        let server = self.config()?.server().pin().to_string();
        let status = self
            .state
            .record(
                self.repository,
                &server,
                &message.channel,
                receipt,
                message.account.as_str(),
                message.sender.did(),
            )
            .await?;
        match status {
            MessageStatus::Handled(_identity) => Ok(None),
            MessageStatus::Pending(identity) => Ok(Some(PendingDelivery { identity, message })),
        }
    }

    async fn deliver_while_receiving(
        &self,
        connection: &mut Connection,
        classifier: &mut Classifier,
        pending: &mut VecDeque<PendingDelivery>,
        delivery: PendingDelivery,
    ) -> Result<(), Error> {
        let identity = delivery.identity;
        let mut delivering = std::pin::pin!(self.delivery.deliver(
            self.repository,
            identity,
            delivery.message,
        ));
        loop {
            tokio::select! {
                result = &mut delivering => {
                    self.complete_delivery(identity, result).await?;
                    self.drain_outbox(connection, classifier).await?;
                    return Ok(());
                }
                received = connection.next() => match received {
                    Ok(message) => {
                        if let Some(delivery) = self.admit(classifier, message, Source::Live).await? {
                            pending.push_back(delivery);
                        }
                    }
                    Err(error) => {
                        let result = delivering.await;
                        self.complete_delivery(identity, result).await?;
                        return Err(error.into());
                    }
                }
            }
        }
    }

    async fn complete_delivery(
        &self,
        identity: MessageId,
        delivered: Result<(), String>,
    ) -> Result<(), Error> {
        delivered.map_err(Error::Delivery)?;
        self.state.mark_handled(identity).await?;

        Ok(())
    }

    async fn drain_outbox(
        &self,
        connection: &mut Connection,
        classifier: &mut Classifier,
    ) -> Result<(), Error> {
        for reply in self.state.pending_replies(self.repository).await? {
            self.send_reply(connection, classifier, reply).await?;
        }
        Ok(())
    }

    async fn send_reply(
        &self,
        connection: &mut Connection,
        classifier: &mut Classifier,
        reply: crate::chat::state::OutboxMessage,
    ) -> Result<(), Error> {
        let channel = self
            .config()?
            .irc_channel(&reply.channel)
            .ok_or_else(|| Error::UnboundOutbox(reply.channel.clone()))?;
        connection.send(channel, &reply.body).await?;
        classifier.replied(&reply.channel);
        self.state.mark_reply_sent(reply.id).await?;
        Ok(())
    }

    fn config(&self) -> Result<&Config, Error> {
        self.policy.chat().ok_or(Error::ChatDisabled)
    }
}

#[async_trait]
pub(crate) trait Delivery: std::fmt::Debug + Send + Sync {
    async fn deliver(
        &self,
        repository: RepoId,
        identity: MessageId,
        message: IncomingMessage,
    ) -> Result<(), String>;
}

#[async_trait]
impl Delivery for dispatcher::Handle {
    async fn deliver(
        &self,
        repository: RepoId,
        identity: MessageId,
        message: IncomingMessage,
    ) -> Result<(), String> {
        self.message(repository, identity, message)
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug)]
struct PendingDelivery {
    identity: MessageId,
    message: IncomingMessage,
}

#[derive(Debug)]
enum DelegateSource {
    Repository(Storage),
    #[cfg(test)]
    Fixed(Vec<Did>),
}

impl DelegateSource {
    async fn resolve(&self, repository: RepoId) -> Result<Vec<Did>, Error> {
        match self {
            Self::Repository(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let repository = storage.repository(repository)?;
                    Ok::<Vec<Did>, radicle::storage::RepositoryError>(
                        repository.delegates()?.into_iter().collect(),
                    )
                })
                .await
                .map_err(|error| Error::Delegates(error.to_string()))?
                .map_err(|error| Error::Delegates(error.to_string()))
            }
            #[cfg(test)]
            Self::Fixed(delegates) => Ok(delegates.clone()),
        }
    }
}

#[derive(Debug)]
struct SessionFailure {
    error: Error,
    connected_for: Option<Duration>,
}

impl SessionFailure {
    fn before_connection(error: Error) -> Self {
        Self {
            error,
            connected_for: None,
        }
    }

    fn connected(error: Error, since: Instant) -> Self {
        Self {
            error,
            connected_for: Some(since.elapsed()),
        }
    }
}

fn next_reconnect_delay(delay: &mut Duration, connected_for: Option<Duration>) -> Duration {
    if connected_for.is_some_and(|duration| duration >= RECONNECT_STABLE) {
        *delay = RECONNECT_INITIAL;
    }
    let current = *delay;
    *delay = (*delay).saturating_mul(2).min(RECONNECT_MAXIMUM);
    current
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Live,
    History,
}

/// Failure running authenticated IRC ingress.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Chat disappeared from the validated repository policy.
    #[error("repository chat configuration is disabled")]
    ChatDisabled,
    /// Opening or using the IRC connection failed.
    #[error(transparent)]
    Transport(#[from] crate::chat::transport::Error),
    /// The connection evidence could not construct a fail-closed classifier.
    #[error(transparent)]
    Classification(#[from] crate::chat::classification::Error),
    /// Durable admission or outbox state failed.
    #[error(transparent)]
    State(#[from] crate::chat::state::Error),
    /// Reading the repository's current delegates failed.
    #[error("failed to resolve repository delegates: {0}")]
    Delegates(String),
    /// The shared talking agent rejected the message.
    #[error("chat delivery failed: {0}")]
    Delivery(String),
    /// Durable outbox state names a channel absent from current policy.
    #[error("chat outbox names unbound channel {0}")]
    UnboundOutbox(crate::chat::ChannelId),
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr as _,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use config::{Config as ConfigLoader, File, FileFormat};
    use irc_proto::Message;
    use radicle::{
        crypto::{Signature, signature::Signer as _, ssh::keystore::MemorySigner},
        identity::{Did, RepoId},
    };
    use rcgen::generate_simple_self_signed;
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use sha2::{Digest as _, Sha256};

    use crate::{
        chat::{
            AttestationPayload, ClientCertificateFingerprint, Config, IrcAccount, MessageId,
            Server, ServerEndpoint, ServerPin, State,
            classification::{Classifier, IncomingMessage},
            runtime::{
                DelegateSource, Delivery, Ingress, RECONNECT_INITIAL, RECONNECT_STABLE, Source,
                next_reconnect_delay,
            },
            transport::{ClientIdentity, VerifiedServer},
        },
        manifest::{Limits, PriorityLabels, RepositoryPolicy, TriggerAccess},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[derive(Debug, Clone, Default)]
    struct RecordingDelivery(Arc<Mutex<Vec<MessageId>>>);

    #[async_trait]
    impl Delivery for RecordingDelivery {
        async fn deliver(
            &self,
            _repository: RepoId,
            identity: MessageId,
            _message: IncomingMessage,
        ) -> Result<(), String> {
            self.0
                .lock()
                .map_err(|error| error.to_string())?
                .push(identity);
            Ok(())
        }
    }

    #[tokio::test]
    async fn replays_each_stable_message_once_across_sessions() {
        let (repository, did, server, config, identity) = fixture();
        let policy = RepositoryPolicy::new(
            TriggerAccess::Anyone,
            vec![did],
            PriorityLabels::default(),
            Limits::default(),
        )
        .with_chat_for_test(config);
        let state = State::memory().await.expect("chat state should open");
        let delivery = RecordingDelivery::default();
        let ingress = Ingress {
            repository,
            member: Member::Nico,
            did,
            policy,
            delegates: DelegateSource::Fixed(vec![did]),
            identity,
            state,
            delivery: delivery.clone(),
        };
        let message = tagged("old-1");

        let mut first = classifier(&ingress, server);
        let first_delivery = ingress
            .admit(&mut first, message.clone(), Source::History)
            .await
            .expect("first replay should be admitted")
            .expect("first replay should need delivery");
        let first_result = ingress
            .delivery
            .deliver(
                ingress.repository,
                first_delivery.identity,
                first_delivery.message,
            )
            .await;
        ingress
            .complete_delivery(first_delivery.identity, first_result)
            .await
            .expect("first replay should complete");
        let mut second = classifier(&ingress, server);
        assert!(
            ingress
                .admit(&mut second, message, Source::History)
                .await
                .expect("second replay should be deduplicated")
                .is_none()
        );
        assert_eq!(
            delivery
                .0
                .lock()
                .expect("recording delivery should be available")
                .len(),
            1
        );

        let mut without_id = classifier(&ingress, server);
        assert!(
            ingress
                .admit(
                    &mut without_id,
                    "@account=alice :alice!u@h PRIVMSG #project :@nico old"
                        .parse()
                        .expect("message should parse"),
                    Source::History,
                )
                .await
                .expect("unstable history should be ignored")
                .is_none()
        );
    }

    fn classifier(ingress: &Ingress<RecordingDelivery>, server: ServerPin) -> Classifier {
        Classifier::from_policy(
            ingress.repository,
            ingress.member,
            ingress.did,
            &ingress.policy,
            VerifiedServer::for_test(server),
            vec![ingress.did],
        )
        .expect("classifier should initialize")
    }

    fn tagged(id: &str) -> Message {
        format!("@account=alice;msgid={id} :alice!u@h PRIVMSG #project :@nico old")
            .parse()
            .expect("message should parse")
    }

    fn fixture() -> (RepoId, Did, ServerPin, Config, ClientIdentity) {
        let repository = REPOSITORY.parse().expect("RID should parse");
        let signer = MemorySigner::r#gen();
        let did = Did::from(*radicle::crypto::Signer::public_key(&signer));
        let alice = MemorySigner::r#gen();
        let alice_did = Did::from(*radicle::crypto::Signer::public_key(&alice));
        let generated = generate_simple_self_signed(vec!["nico".to_owned()])
            .expect("certificate should be generated");
        let certificate = generated.cert.der().clone();
        let fingerprint =
            ClientCertificateFingerprint::from_digest(Sha256::digest(certificate.as_ref()).into());
        let server_pin = ServerPin::from_digest([7; 32]);
        let server = Server::new(
            ServerEndpoint::from_str("ircs://127.0.0.1:6697").expect("endpoint should parse"),
            server_pin,
        );
        let account = IrcAccount::from_str("nico").expect("account should parse");
        let payload = AttestationPayload::new(
            repository,
            server.clone(),
            account.clone(),
            fingerprint,
            did,
        );
        let signature: Signature = signer.sign(payload.canonical().as_bytes());
        let alice_account = IrcAccount::from_str("alice").expect("account should parse");
        let alice_fingerprint = ClientCertificateFingerprint::from_digest([8; 32]);
        let alice_payload = AttestationPayload::new(
            repository,
            server.clone(),
            alice_account.clone(),
            alice_fingerprint,
            alice_did,
        );
        let alice_signature: Signature = alice.sign(alice_payload.canonical().as_bytes());
        let document = format!(
            concat!(
                "repository = \"{}\"\n",
                "[server]\nendpoint = \"{}\"\npin = \"{}\"\n",
                "[channels]\nteam = \"#project\"\n",
                "[[participants]]\naccount = \"{}\"\ncertificate = \"{}\"\n",
                "did = \"{}\"\nsignature = \"{}\"\n",
                "[[participants]]\naccount = \"{}\"\ncertificate = \"{}\"\n",
                "did = \"{}\"\nsignature = \"{}\"\n",
            ),
            repository,
            server.endpoint(),
            server.pin(),
            account,
            fingerprint,
            did,
            signature,
            alice_account,
            alice_fingerprint,
            alice_did,
            alice_signature,
        );
        let config = ConfigLoader::builder()
            .add_source(File::from_str(&document, FileFormat::Toml))
            .build()
            .expect("fixture should load")
            .try_deserialize::<Config>()
            .expect("fixture should deserialize")
            .validate(&[did])
            .expect("fixture should validate");
        let identity = ClientIdentity::new(
            vec![certificate],
            PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
        )
        .expect("client identity should build");

        (repository, did, server_pin, config, identity)
    }

    #[test]
    fn reconnect_starts_with_a_short_delay() {
        assert_eq!(RECONNECT_INITIAL, std::time::Duration::from_millis(250));
    }

    #[test]
    fn reconnect_backoff_resets_only_after_a_stable_session() {
        let mut delay = RECONNECT_INITIAL;

        assert_eq!(next_reconnect_delay(&mut delay, None), RECONNECT_INITIAL);
        assert_eq!(
            next_reconnect_delay(&mut delay, Some(Duration::ZERO)),
            Duration::from_millis(500)
        );
        assert_eq!(
            next_reconnect_delay(&mut delay, Some(RECONNECT_STABLE)),
            RECONNECT_INITIAL
        );
    }
}
