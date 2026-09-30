//! Fail-closed reduction of IRC messages into authenticated chat work.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the IRC ingress is isolated behind its transport and classifier boundary"
    )
)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    str::FromStr as _,
};

use irc_proto::{Command, Message};
use radicle::identity::{Did, RepoId};

use crate::{
    chat::{
        AuthenticatedSender, ChannelId, Config, IrcAccount, ServerPin, transport::VerifiedServer,
    },
    conversation::{AdmissionError, Author, Triggers, Turn},
    manifest::{Limits, RepositoryPolicy, TriggerAccess},
    member::Member,
};

/// An authenticated, addressed message ready for durable admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncomingMessage {
    pub(crate) channel: ChannelId,
    pub(crate) network_id: Option<String>,
    pub(crate) account: IrcAccount,
    pub(crate) sender: AuthenticatedSender,
    pub(crate) nick: String,
    pub(crate) body: String,
}

/// Result of applying identity, channel, policy, addressing, and conversation limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Classification {
    /// The message may create agent work.
    Relevant(IncomingMessage),
    /// The message must not create agent work.
    Ignored(Ignored),
}

/// Why an IRC message did not become work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ignored {
    NotChannelMessage,
    UnboundChannel,
    MissingNick,
    MissingAccount,
    AmbiguousAccount,
    UnattestedAccount,
    OwnMessage,
    UnauthorizedSender,
    NotAddressed,
    AmbiguousMessageId,
    DuplicateMessage,
    ConversationLimit,
}

/// Stateful classifier for one member and repository.
#[derive(Debug)]
pub(crate) struct Classifier {
    repository: RepoId,
    member: Member,
    member_did: Did,
    config: Config,
    server: ServerPin,
    triggers: Triggers,
    limits: Limits,
    turns: BTreeMap<ChannelId, Turn>,
    seen: Seen,
}

impl Classifier {
    /// Creates a classifier only from a connection that proved the manifest's server pin.
    pub(crate) fn from_policy(
        repository: RepoId,
        member: Member,
        member_did: Did,
        policy: &RepositoryPolicy,
        server: VerifiedServer,
        delegates: Vec<Did>,
    ) -> Result<Self, Error> {
        Self::new(
            repository,
            member,
            member_did,
            policy.chat().ok_or(Error::ChatDisabled)?.clone(),
            server,
            policy.access().clone(),
            policy.members().to_vec(),
            delegates,
            policy.limits().clone(),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is an independent trust input"
    )]
    fn new(
        repository: RepoId,
        member: Member,
        member_did: Did,
        config: Config,
        server: VerifiedServer,
        access: TriggerAccess,
        members: Vec<Did>,
        delegates: Vec<Did>,
        limits: Limits,
    ) -> Result<Self, Error> {
        if config.repository() != repository {
            return Err(Error::Repository);
        }
        if config.server().pin() != server.pin() {
            return Err(Error::Server);
        }
        if !members.contains(&member_did) || config.participant(member_did).is_none() {
            return Err(Error::Member);
        }
        Ok(Self {
            repository,
            member,
            member_did,
            config,
            server: server.pin(),
            triggers: Triggers::new(access, delegates, members),
            limits,
            turns: BTreeMap::new(),
            seen: Seen::default(),
        })
    }

    /// Refreshes the repository delegate set used for sender authorization.
    pub(crate) fn update_delegates(&mut self, delegates: Vec<Did>) {
        self.triggers.update_delegates(delegates);
    }

    /// Records that this member replied without depending on server echo support.
    pub(crate) fn replied(&mut self, channel: &ChannelId) {
        if let Some(turn) = self.turns.get_mut(channel) {
            turn.begin(self.member_did);
        }
    }

    /// Classifies one decoded IRC message without ever falling back to its nick for identity.
    pub(crate) fn classify(&mut self, message: &Message) -> Classification {
        let Command::PRIVMSG(target, body) = &message.command else {
            return Classification::Ignored(Ignored::NotChannelMessage);
        };
        let Some(channel) = self
            .config
            .channels()
            .find_map(|(logical, irc)| (irc.as_str() == target).then(|| logical.clone()))
        else {
            return Classification::Ignored(Ignored::UnboundChannel);
        };
        let Some(nick) = message.source_nickname() else {
            return Classification::Ignored(Ignored::MissingNick);
        };

        let account = match unique_tag(message, "account") {
            TagValue::Missing => return Classification::Ignored(Ignored::MissingAccount),
            TagValue::Ambiguous => return Classification::Ignored(Ignored::AmbiguousAccount),
            TagValue::Present(account) => account,
        };
        let Ok(account) = IrcAccount::from_str(account) else {
            return Classification::Ignored(Ignored::UnattestedAccount);
        };
        let Ok(sender) = self
            .config
            .authenticate(self.repository, self.server, &account)
        else {
            return Classification::Ignored(Ignored::UnattestedAccount);
        };
        let author = sender.did();
        let network_id = match unique_tag(message, "msgid") {
            TagValue::Missing => None,
            TagValue::Ambiguous => {
                return Classification::Ignored(Ignored::AmbiguousMessageId);
            }
            TagValue::Present(message) => Some(message.to_owned()),
        };
        let origin = self.triggers.author(author, self.member_did);
        let limits = self.limits.clone();
        let turn = self
            .turns
            .entry(channel.clone())
            .or_insert_with(|| Turn::new(limits));
        turn.begin(author);
        if origin == Author::Member {
            return Classification::Ignored(Ignored::OwnMessage);
        }

        if origin == Author::Stranger {
            return Classification::Ignored(Ignored::UnauthorizedSender);
        }
        if !origin.addresses(self.member, body) {
            return Classification::Ignored(Ignored::NotAddressed);
        }

        if network_id
            .as_ref()
            .is_some_and(|message| !self.seen.insert((channel.clone(), message.clone())))
        {
            return Classification::Ignored(Ignored::DuplicateMessage);
        }

        if let Err(error) = turn.admit(author, origin) {
            return Classification::Ignored(match error {
                AdmissionError::Unauthorized => Ignored::UnauthorizedSender,
                AdmissionError::Limit => Ignored::ConversationLimit,
            });
        }

        Classification::Relevant(IncomingMessage {
            channel,
            network_id,
            account,
            sender,
            nick: nick.to_owned(),
            body: body.clone(),
        })
    }
}

const SEEN_CAPACITY: usize = 8_192;

#[derive(Debug, Default)]
struct Seen {
    entries: BTreeSet<(ChannelId, String)>,
    order: VecDeque<(ChannelId, String)>,
}

impl Seen {
    fn insert(&mut self, identity: (ChannelId, String)) -> bool {
        if !self.entries.insert(identity.clone()) {
            return false;
        }
        self.order.push_back(identity);
        if self.order.len() > SEEN_CAPACITY
            && let Some(expired) = self.order.pop_front()
        {
            let _removed = self.entries.remove(&expired);
        }
        true
    }
}

enum TagValue<'a> {
    Missing,
    Present(&'a str),
    Ambiguous,
}

fn unique_tag<'a>(message: &'a Message, name: &str) -> TagValue<'a> {
    let mut values = message
        .tags
        .iter()
        .flatten()
        .filter(|tag| tag.0 == name)
        .map(|tag| tag.1.as_deref());
    match (values.next(), values.next()) {
        (None, _) | (Some(None | Some("")), None) => TagValue::Missing,
        (Some(Some(value)), None) => TagValue::Present(value),
        (Some(_), Some(_)) => TagValue::Ambiguous,
    }
}

/// Invalid trust inputs supplied when constructing a classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    /// The repository has no chat configuration.
    #[error("chat classifier is disabled by the repository manifest")]
    ChatDisabled,
    /// The configuration belongs to another repository.
    #[error("chat classifier configuration belongs to another repository")]
    Repository,
    /// The connection did not prove the configured server.
    #[error("chat classifier connection does not match the configured server pin")]
    Server,
    /// The selected member is not active and attested.
    #[error("chat classifier member is not active and attested")]
    Member,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use config::{Config as ConfigLoader, File, FileFormat};
    use irc_proto::Message;
    use radicle::{
        crypto::{Signature, signature::Signer as _, ssh::keystore::MemorySigner},
        identity::{Did, RepoId},
    };

    use crate::{
        chat::{
            AttestationPayload, ChannelId, ClientCertificateFingerprint, Config, IrcAccount,
            Server, ServerPin,
            classification::{Classification, Classifier, Error, Ignored, SEEN_CAPACITY, Seen},
            transport::VerifiedServer,
        },
        manifest::{Limits, PriorityLabels, RepositoryPolicy, TriggerAccess},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const ENDPOINT: &str = "ircs://irc.example.com:6697";
    const SERVER_PIN: &str =
        "spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct Fixture {
        repository: RepoId,
        server: ServerPin,
        config: Config,
        nico: Did,
        maya: Did,
        alice: Did,
    }

    impl Fixture {
        fn new() -> Self {
            let repository = REPOSITORY.parse().expect("RID should parse");
            let server_pin = SERVER_PIN.parse().expect("server pin should parse");
            let server = Server::new(ENDPOINT.parse().expect("endpoint should parse"), server_pin);
            let nico = MemorySigner::r#gen();
            let maya = MemorySigner::r#gen();
            let alice = MemorySigner::r#gen();
            let participants = [
                participant(&nico, repository, &server, "nico", 1),
                participant(&maya, repository, &server, "maya", 2),
                participant(&alice, repository, &server, "alice", 3),
            ]
            .join("\n");
            let document = format!(
                concat!(
                    "repository = \"{}\"\n",
                    "[server]\n",
                    "endpoint = \"{}\"\n",
                    "pin = \"{}\"\n",
                    "[channels]\n",
                    "team = \"#project\"\n",
                    "security = \"#security\"\n",
                    "{}\n",
                ),
                REPOSITORY, ENDPOINT, SERVER_PIN, participants,
            );
            let nico_did = did(&nico);
            let maya_did = did(&maya);
            let alice_did = did(&alice);
            let config = ConfigLoader::builder()
                .add_source(File::from_str(&document, FileFormat::Toml))
                .build()
                .expect("chat fixture should load")
                .try_deserialize::<Config>()
                .expect("chat fixture should deserialize")
                .validate(&[nico_did, maya_did])
                .expect("chat fixture should validate");

            Self {
                repository,
                server: server_pin,
                config,
                nico: nico_did,
                maya: maya_did,
                alice: alice_did,
            }
        }

        fn classifier(&self, access: TriggerAccess) -> Classifier {
            Classifier::new(
                self.repository,
                Member::Nico,
                self.nico,
                self.config.clone(),
                VerifiedServer::for_test(self.server),
                access,
                vec![self.nico, self.maya],
                Vec::new(),
                Limits::default(),
            )
            .expect("classifier should initialize")
        }
    }

    fn did(signer: &MemorySigner) -> Did {
        Did::from(*radicle::crypto::Signer::public_key(signer))
    }

    fn participant(
        signer: &MemorySigner,
        repository: RepoId,
        server: &Server,
        account: &str,
        certificate_byte: u8,
    ) -> String {
        let account = IrcAccount::from_str(account).expect("account should parse");
        let certificate = ClientCertificateFingerprint::from_digest([certificate_byte; 32]);
        let participant = did(signer);
        let payload = AttestationPayload::new(
            repository,
            server.clone(),
            account.clone(),
            certificate,
            participant,
        );
        let signature: Signature = signer.sign(payload.canonical().as_bytes());
        format!(
            concat!(
                "[[participants]]\n",
                "account = \"{}\"\n",
                "certificate = \"{}\"\n",
                "did = \"{}\"\n",
                "signature = \"{}\"\n",
            ),
            account, certificate, participant, signature,
        )
    }

    fn message(account: Option<&str>, id: Option<&str>, target: &str, body: &str) -> Message {
        let tags = [
            account.map(|value| format!("account={value}")),
            id.map(|value| format!("msgid={value}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(";");
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("@{tags} ")
        };
        format!("{tags}:sender!user@host PRIVMSG {target} :{body}\r\n")
            .parse()
            .expect("message fixture should parse")
    }

    #[test]
    fn authenticates_channels_accounts_and_addressing() {
        let fixture = Fixture::new();
        let mut classifier = fixture.classifier(TriggerAccess::Anyone);

        let classification = classifier.classify(&message(
            Some("alice"),
            Some("message-1"),
            "#project",
            "@nico please inspect this",
        ));
        assert!(matches!(classification, Classification::Relevant(_)));
        if let Classification::Relevant(relevant) = classification {
            assert_eq!(relevant.channel.as_str(), "team");
            assert_eq!(relevant.network_id.as_deref(), Some("message-1"));
            assert_eq!(relevant.account.as_str(), "alice");
            assert_eq!(relevant.sender.did(), fixture.alice);
            assert_eq!(relevant.nick, "sender");
            assert_eq!(relevant.body, "@nico please inspect this");
        }

        assert_eq!(
            classifier.classify(&message(None, None, "#project", "@nico hello")),
            Classification::Ignored(Ignored::MissingAccount)
        );
        assert_eq!(
            classifier.classify(&message(Some("mallory"), None, "#project", "@nico hello")),
            Classification::Ignored(Ignored::UnattestedAccount)
        );
        assert_eq!(
            classifier.classify(&message(Some("alice"), None, "#other", "@nico hello")),
            Classification::Ignored(Ignored::UnboundChannel)
        );
        let seen = classifier.seen.entries.len();
        assert_eq!(
            classifier.classify(&message(
                Some("alice"),
                Some("ignored"),
                "#project",
                "hello",
            )),
            Classification::Ignored(Ignored::NotAddressed)
        );
        assert_eq!(classifier.seen.entries.len(), seen);
        assert_eq!(
            classifier.classify(&message(Some("nico"), None, "#project", "@nico echo")),
            Classification::Ignored(Ignored::OwnMessage)
        );
    }

    #[test]
    fn enforces_policy_and_peer_addressing() {
        let fixture = Fixture::new();
        let mut restricted = fixture.classifier(TriggerAccess::Delegates);
        assert_eq!(
            restricted.classify(&message(Some("alice"), None, "#project", "@nico hello")),
            Classification::Ignored(Ignored::UnauthorizedSender)
        );

        let mut classifier = fixture.classifier(TriggerAccess::Anyone);
        assert_eq!(
            classifier.classify(&message(Some("maya"), None, "#project", "@team hello")),
            Classification::Ignored(Ignored::NotAddressed)
        );
        assert!(matches!(
            classifier.classify(&message(Some("maya"), None, "#project", "@nico hello")),
            Classification::Relevant(_)
        ));
        assert!(matches!(
            classifier.classify(&message(Some("alice"), None, "#project", "@team hello")),
            Classification::Relevant(_)
        ));
    }

    #[test]
    fn limits_peers_per_channel_until_a_person_addresses_the_member() {
        let fixture = Fixture::new();
        let mut classifier = fixture.classifier(TriggerAccess::Anyone);

        for sequence in 1..=Limits::default().exchanges() {
            assert!(matches!(
                classifier.classify(&message(
                    Some("maya"),
                    Some(&format!("peer-{sequence}")),
                    "#project",
                    "@nico continue",
                )),
                Classification::Relevant(_)
            ));
            classifier.replied(&ChannelId::from_str("team").expect("channel should parse"));
        }
        assert_eq!(
            classifier.classify(&message(
                Some("maya"),
                Some("peer-over-limit"),
                "#project",
                "@nico continue",
            )),
            Classification::Ignored(Ignored::ConversationLimit)
        );
        assert!(matches!(
            classifier.classify(&message(
                Some("maya"),
                Some("other-channel"),
                "#security",
                "@nico continue",
            )),
            Classification::Relevant(_)
        ));
        assert!(matches!(
            classifier.classify(&message(
                Some("alice"),
                Some("person"),
                "#project",
                "@nico please continue",
            )),
            Classification::Relevant(_)
        ));
        assert!(matches!(
            classifier.classify(&message(
                Some("maya"),
                Some("peer-after-reset"),
                "#project",
                "@nico continue",
            )),
            Classification::Relevant(_)
        ));
    }

    #[test]
    fn counts_a_run_of_peer_messages_as_one_turn() {
        let fixture = Fixture::new();
        let mut classifier = fixture.classifier(TriggerAccess::Anyone);

        for sequence in 0..=Limits::default().exchanges() {
            assert!(matches!(
                classifier.classify(&message(
                    Some("maya"),
                    Some(&format!("same-turn-{sequence}")),
                    "#project",
                    "@nico continue",
                )),
                Classification::Relevant(_)
            ));
        }
    }

    #[test]
    fn refreshes_delegates_without_reconnecting() {
        let fixture = Fixture::new();
        let mut classifier = fixture.classifier(TriggerAccess::Delegates);

        assert_eq!(
            classifier.classify(&message(
                Some("alice"),
                Some("before"),
                "#project",
                "@nico hi"
            )),
            Classification::Ignored(Ignored::UnauthorizedSender)
        );
        classifier.update_delegates(vec![fixture.alice]);
        assert!(matches!(
            classifier.classify(&message(
                Some("alice"),
                Some("during"),
                "#project",
                "@nico hi"
            )),
            Classification::Relevant(_)
        ));
        classifier.update_delegates(Vec::new());
        assert_eq!(
            classifier.classify(&message(
                Some("alice"),
                Some("after"),
                "#project",
                "@nico hi"
            )),
            Classification::Ignored(Ignored::UnauthorizedSender)
        );
    }

    #[test]
    fn bounds_recent_message_deduplication() {
        let channel = ChannelId::from_str("team").expect("channel should parse");
        let mut seen = Seen::default();

        for sequence in 0..=SEEN_CAPACITY {
            assert!(seen.insert((channel.clone(), format!("message-{sequence}"))));
        }

        assert_eq!(seen.entries.len(), SEEN_CAPACITY);
        assert_eq!(seen.order.len(), SEEN_CAPACITY);
    }

    #[test]
    fn rejects_ambiguous_identity_and_message_tags() {
        let fixture = Fixture::new();
        let mut classifier = fixture.classifier(TriggerAccess::Anyone);
        let duplicate_account = "@account=alice;account=maya :sender!u@h PRIVMSG #project :@nico \
                                 hi\r\n"
            .parse::<Message>()
            .expect("fixture should parse");
        let duplicate_id = "@account=alice;msgid=one;msgid=two :sender!u@h PRIVMSG #project \
                            :@nico hi\r\n"
            .parse::<Message>()
            .expect("fixture should parse");

        assert_eq!(
            classifier.classify(&duplicate_account),
            Classification::Ignored(Ignored::AmbiguousAccount)
        );
        assert_eq!(
            classifier.classify(&duplicate_id),
            Classification::Ignored(Ignored::AmbiguousMessageId)
        );
    }

    #[test]
    fn constructor_rejects_unproven_server_and_member() {
        let fixture = Fixture::new();
        let another_server = VerifiedServer::for_test(ServerPin::from_digest([9; 32]));
        Classifier::new(
            fixture.repository,
            Member::Nico,
            fixture.nico,
            fixture.config.clone(),
            another_server,
            TriggerAccess::Anyone,
            vec![fixture.nico, fixture.maya],
            Vec::new(),
            Limits::default(),
        )
        .expect_err("another server must be rejected");
        let unknown = MemorySigner::r#gen();
        Classifier::new(
            fixture.repository,
            Member::Nico,
            did(&unknown),
            fixture.config,
            VerifiedServer::for_test(fixture.server),
            TriggerAccess::Anyone,
            vec![fixture.nico, fixture.maya],
            Vec::new(),
            Limits::default(),
        )
        .expect_err("an unattested member must be rejected");

        let policy = RepositoryPolicy::new(
            TriggerAccess::Anyone,
            vec![fixture.nico, fixture.maya],
            PriorityLabels::default(),
            Limits::default(),
        );
        assert!(matches!(
            Classifier::from_policy(
                fixture.repository,
                Member::Nico,
                fixture.nico,
                &policy,
                VerifiedServer::for_test(fixture.server),
                Vec::new(),
            ),
            Err(Error::ChatDisabled)
        ));
    }
}
