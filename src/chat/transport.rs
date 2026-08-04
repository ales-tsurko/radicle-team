//! Pinned TLS transport and IRCv3 session establishment.

#![cfg_attr(
    any(not(test), miri),
    expect(
        dead_code,
        reason = "the IRC connection is exposed only through the chat ingress boundary"
    )
)]
#![cfg_attr(
    all(test, not(miri)),
    expect(
        dead_code,
        reason = "the concrete TCP connection is covered through its protocol and TLS components"
    )
)]

use std::{
    collections::{BTreeSet, VecDeque},
    fmt, io,
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use irc_proto::{CapSubCommand, Command, IrcCodec, Message, Response};
use radicle::identity::Did;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{
        WebPkiSupportedAlgorithms, aws_lc_rs, verify_tls12_signature, verify_tls13_signature,
    },
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::{
    bytes::BytesMut,
    codec::{Decoder, Encoder, Framed},
};

use crate::{
    chat::{ClientCertificateFingerprint, Config, IrcAccount, IrcChannel, Server, ServerPin},
    member::Member,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAXIMUM_LINE_BYTES: usize = 16 * 1024;
const MAXIMUM_REPLY_BODY_BYTES: usize = 400;
const HISTORY_LIMIT: usize = 1_000;

/// Client certificate chain and private key used by SASL `EXTERNAL`.
pub(crate) struct ClientIdentity {
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
}

impl Clone for ClientIdentity {
    fn clone(&self) -> Self {
        Self {
            certificates: self.certificates.clone(),
            private_key: self.private_key.clone_key(),
        }
    }
}

impl ClientIdentity {
    /// Creates a TLS identity and rejects an empty certificate chain.
    pub(crate) fn new(
        certificates: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
    ) -> Result<Self, Error> {
        if certificates.is_empty() {
            return Err(Error::MissingClientCertificate);
        }
        Ok(Self {
            certificates,
            private_key,
        })
    }

    fn verify_fingerprint(&self, expected: ClientCertificateFingerprint) -> Result<(), Error> {
        let certificate = self
            .certificates
            .first()
            .ok_or(Error::MissingClientCertificate)?;
        let actual = Sha256::digest(certificate.as_ref());
        if actual.as_slice() == expected.digest() {
            Ok(())
        } else {
            Err(Error::ClientCertificateMismatch)
        }
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientIdentity")
            .field("certificates", &self.certificates.len())
            .field("private_key", &"[redacted]")
            .finish()
    }
}

/// Evidence that a connection completed TLS using the manifest pin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VerifiedServer(ServerPin);

impl VerifiedServer {
    pub(super) const fn pin(self) -> ServerPin {
        self.0
    }

    #[cfg(all(test, not(miri)))]
    pub(super) const fn for_test(pin: ServerPin) -> Self {
        Self(pin)
    }
}

/// One authenticated IRC connection over pinned TLS.
pub(crate) struct Connection {
    wire: Framed<TlsStream<TcpStream>, WireCodec>,
    server: VerifiedServer,
    channels: BTreeSet<IrcChannel>,
    history: bool,
    pending: VecDeque<Message>,
}

impl Connection {
    /// Connects, checks the exact server SPKI and client certificate, authenticates with SASL
    /// `EXTERNAL`, and joins the configured channels.
    pub(crate) async fn connect(
        config: &Config,
        member: Member,
        did: Did,
        identity: ClientIdentity,
    ) -> Result<Self, Error> {
        let participant = config
            .participant(did)
            .ok_or(Error::MissingAttestation(did))?;
        identity.verify_fingerprint(participant.certificate())?;
        let nickname = member.id();
        let channels = config
            .channels()
            .map(|(_logical, channel)| channel.clone())
            .collect::<BTreeSet<_>>();
        if channels.is_empty() {
            return Err(Error::MissingChannels);
        }

        tokio::time::timeout(
            CONNECT_TIMEOUT,
            Self::connect_inner(
                config.server(),
                participant.account(),
                nickname,
                channels,
                identity,
            ),
        )
        .await
        .map_err(|_elapsed| Error::Timeout)?
    }

    async fn connect_inner(
        server: &Server,
        account: &IrcAccount,
        nickname: &str,
        channels: BTreeSet<IrcChannel>,
        identity: ClientIdentity,
    ) -> Result<Self, Error> {
        let endpoint = server.endpoint();
        let tcp = TcpStream::connect((endpoint.host(), endpoint.port())).await?;
        let server_name = ServerName::try_from(endpoint.host().to_owned())
            .map_err(|_error| Error::InvalidServerName(endpoint.host().to_owned()))?;
        let tls = TlsConnector::from(Arc::new(tls_configuration(server.pin(), identity)?))
            .connect(server_name, tcp)
            .await?;
        let mut wire = Framed::new(tls, WireCodec::new()?);
        let history = register(&mut wire, account, nickname, &channels).await?;

        Ok(Self {
            wire,
            server: VerifiedServer(server.pin()),
            channels,
            history,
            pending: VecDeque::new(),
        })
    }

    /// Returns evidence for the server authenticated by this connection.
    pub(super) const fn server(&self) -> VerifiedServer {
        self.server
    }

    /// Returns whether server history was negotiated.
    pub(crate) const fn has_history(&self) -> bool {
        self.history
    }

    /// Requests the latest retained messages for one manifest-bound channel.
    pub(crate) async fn history(&mut self, channel: &IrcChannel) -> Result<Vec<Message>, Error> {
        if !self.history {
            return Ok(Vec::new());
        }
        if !self.channels.contains(channel) {
            return Err(Error::UnboundChannel(channel.clone()));
        }
        self.wire
            .send(
                Command::Raw(
                    "CHATHISTORY".to_owned(),
                    vec![
                        "LATEST".to_owned(),
                        channel.to_string(),
                        "*".to_owned(),
                        HISTORY_LIMIT.to_string(),
                    ],
                )
                .into(),
            )
            .await?;
        tokio::time::timeout(CONNECT_TIMEOUT, self.receive_history(channel))
            .await
            .map_err(|_elapsed| Error::Timeout)?
    }

    /// Receives the next non-keepalive IRC message.
    pub(crate) async fn next(&mut self) -> Result<Message, Error> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        self.next_wire().await
    }

    async fn next_wire(&mut self) -> Result<Message, Error> {
        loop {
            let message = self.wire.next().await.ok_or(Error::Disconnected)??;
            match message.command {
                Command::PING(server, second) => {
                    self.wire.send(Command::PONG(server, second).into()).await?;
                }
                Command::CAP(_, CapSubCommand::DEL, parameter, trailing)
                    if capability_names(parameter.as_deref(), trailing.as_deref())
                        .any(|capability| capability == "account-tag") =>
                {
                    return Err(Error::RequiredCapabilityLost("account-tag"));
                }
                _ => return Ok(message),
            }
        }
    }

    async fn receive_history(&mut self, channel: &IrcChannel) -> Result<Vec<Message>, Error> {
        let mut batch = None;
        let mut messages = Vec::new();
        loop {
            let message = self.next_wire().await?;
            match &message.command {
                Command::BATCH(reference, Some(kind), parameters)
                    if reference.starts_with('+')
                        && kind.to_str().eq_ignore_ascii_case("chathistory")
                        && parameters
                            .as_ref()
                            .and_then(|parameters| parameters.first())
                            .is_some_and(|target| target == channel.as_str()) =>
                {
                    batch = Some(reference.trim_start_matches('+').to_owned());
                }
                Command::BATCH(reference, _, _)
                    if batch
                        .as_deref()
                        .is_some_and(|batch| reference.strip_prefix('-') == Some(batch)) =>
                {
                    return Ok(messages);
                }
                Command::Raw(command, parameters)
                    if command.eq_ignore_ascii_case("FAIL")
                        && parameters
                            .first()
                            .is_some_and(|command| command.eq_ignore_ascii_case("CHATHISTORY")) =>
                {
                    return Err(Error::History(parameters.join(" ")));
                }
                _ if batch
                    .as_deref()
                    .is_some_and(|batch| message_tag(&message, "batch") == Some(batch)) =>
                {
                    messages.push(message);
                }
                _ => self.pending.push_back(message),
            }
        }
    }

    /// Sends a reply only to a channel joined from the manifest.
    pub(crate) async fn send(&mut self, channel: &IrcChannel, body: &str) -> Result<(), Error> {
        if !self.channels.contains(channel) {
            return Err(Error::UnboundChannel(channel.clone()));
        }
        for part in reply_parts(body)? {
            self.wire
                .send(Command::PRIVMSG(channel.to_string(), part).into())
                .await?;
        }
        Ok(())
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("server", &self.server)
            .field("channels", &self.channels)
            .field("history", &self.history)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

fn tls_configuration(pin: ServerPin, identity: ClientIdentity) -> Result<ClientConfig, Error> {
    let provider = aws_lc_rs::default_provider();
    let algorithms = WebPkiSupportedAlgorithms {
        all: provider.signature_verification_algorithms.all,
        mapping: provider.signature_verification_algorithms.mapping,
    };
    let verifier = Arc::new(PinnedServerVerifier { pin, algorithms });
    ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(identity.certificates, identity.private_key)
        .map_err(Error::from)
}

struct PinnedServerVerifier {
    pin: ServerPin,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedServerVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedServerVerifier")
            .field("pin", &self.pin)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // The manifest pins the exact server key, so hostname, chain, validity, and revocation do
        // not participate in identity verification. Handshake signatures are still checked below.
        let certificate = ParsedCertificate::try_from(end_entity)
            .map_err(|_error| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        let actual = Sha256::digest(certificate.subject_public_key_info().as_ref());
        if actual.as_slice() == self.pin.digest() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

async fn register<S>(
    wire: &mut Framed<S, WireCodec>,
    account: &IrcAccount,
    nickname: &str,
    channels: &BTreeSet<IrcChannel>,
) -> Result<bool, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    begin_registration(wire, account, nickname).await?;
    let history = negotiate_capabilities(wire).await?;
    authenticate_external(wire, account).await?;
    complete_registration(wire, channels).await?;

    Ok(history)
}

async fn begin_registration<S>(
    wire: &mut Framed<S, WireCodec>,
    account: &IrcAccount,
    nickname: &str,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    wire.send(Command::CAP(None, CapSubCommand::LS, Some("302".to_owned()), None).into())
        .await?;
    wire.send(Command::NICK(nickname.to_owned()).into()).await?;
    wire.send(
        Command::USER(
            nickname.to_owned(),
            "0".to_owned(),
            format!("radicle-team {account}"),
        )
        .into(),
    )
    .await?;
    Ok(())
}

async fn negotiate_capabilities<S>(wire: &mut Framed<S, WireCodec>) -> Result<bool, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let offered = receive_capabilities(wire).await?;
    require_capabilities(&offered)?;
    let mut identity = vec!["sasl", "account-tag"];
    let message_tags = offered.contains("message-tags");
    if message_tags {
        identity.push("message-tags");
    }
    request_capabilities(wire, &identity, true).await?;
    let history = ["draft/chathistory", "chathistory"]
        .into_iter()
        .find(|capability| offered.contains(*capability))
        .filter(|_capability| offered.contains("batch") && message_tags);
    let history = if let Some(history) = history {
        request_capabilities(wire, &[history, "batch"], false).await?
    } else {
        false
    };
    Ok(history)
}

fn reply_parts(body: &str) -> Result<Vec<String>, Error> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut parts = Vec::new();
    for line in normalized.split('\n').filter(|line| !line.is_empty()) {
        let mut part = String::new();
        for character in line.chars() {
            if !part.is_empty()
                && part.len().saturating_add(character.len_utf8()) > MAXIMUM_REPLY_BODY_BYTES
            {
                parts.push(std::mem::take(&mut part));
            }
            part.push(character);
        }
        if !part.is_empty() {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        Err(Error::InvalidMessageBody)
    } else {
        Ok(parts)
    }
}

fn require_capabilities(offered: &BTreeSet<String>) -> Result<(), Error> {
    for required in ["sasl", "account-tag"] {
        if !offered.contains(required) {
            return Err(Error::MissingCapability(required));
        }
    }
    Ok(())
}

async fn authenticate_external<S>(
    wire: &mut Framed<S, WireCodec>,
    account: &IrcAccount,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    wire.send(Command::AUTHENTICATE("EXTERNAL".to_owned()).into())
        .await?;
    receive_authentication(wire, SaslStep::Challenge).await?;
    let authorization_identity = base64::engine::general_purpose::STANDARD.encode(account.as_str());
    wire.send(Command::AUTHENTICATE(authorization_identity).into())
        .await?;
    receive_authentication(wire, SaslStep::Success).await
}

#[derive(Debug, Clone, Copy)]
enum SaslStep {
    Challenge,
    Success,
}

impl SaslStep {
    fn completed_by(self, command: &Command) -> bool {
        match self {
            Self::Challenge => {
                matches!(command, Command::AUTHENTICATE(challenge) if challenge == "+")
            }
            Self::Success => {
                matches!(command, Command::Response(Response::RPL_SASLSUCCESS, _))
            }
        }
    }
}

async fn receive_authentication<S>(
    wire: &mut Framed<S, WireCodec>,
    step: SaslStep,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = receive_registration_message(wire).await?;
        if step.completed_by(&message.command) {
            return Ok(());
        }
        if matches!(message.command, Command::Response(response, _) if is_sasl_failure(response)) {
            return Err(Error::SaslAuthentication);
        }
    }
}

async fn complete_registration<S>(
    wire: &mut Framed<S, WireCodec>,
    channels: &BTreeSet<IrcChannel>,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    wire.send(Command::CAP(None, CapSubCommand::END, None, None).into())
        .await?;
    receive_welcome(wire).await?;
    join_channels(wire, channels).await
}

async fn receive_welcome<S>(wire: &mut Framed<S, WireCodec>) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = receive_registration_message(wire).await?;
        if matches!(message.command, Command::Response(Response::RPL_WELCOME, _)) {
            return Ok(());
        }
    }
}

async fn join_channels<S>(
    wire: &mut Framed<S, WireCodec>,
    channels: &BTreeSet<IrcChannel>,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for channel in channels {
        wire.send(Command::JOIN(channel.to_string(), None, None).into())
            .await?;
    }
    Ok(())
}

async fn request_capabilities<S>(
    wire: &mut Framed<S, WireCodec>,
    capabilities: &[&str],
    required: bool,
) -> Result<bool, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    wire.send(Command::CAP(None, CapSubCommand::REQ, None, Some(capabilities.join(" "))).into())
        .await?;
    receive_capability_acknowledgement(wire, capabilities, required).await
}

async fn receive_capabilities<S>(wire: &mut Framed<S, WireCodec>) -> Result<BTreeSet<String>, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut offered = BTreeSet::new();
    loop {
        let message = receive_registration_message(wire).await?;
        if let Command::CAP(_, CapSubCommand::LS, parameter, trailing) = message.command {
            offered.extend(
                capability_names(parameter.as_deref(), trailing.as_deref()).map(str::to_owned),
            );
            if trailing.is_none() || parameter.as_deref() != Some("*") {
                return Ok(offered);
            }
        }
    }
}

async fn receive_capability_acknowledgement<S>(
    wire: &mut Framed<S, WireCodec>,
    requested: &[&str],
    required: bool,
) -> Result<bool, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = receive_registration_message(wire).await?;
        match message.command {
            Command::CAP(_, CapSubCommand::ACK, parameter, trailing) => {
                let acknowledged = capability_names(parameter.as_deref(), trailing.as_deref())
                    .collect::<BTreeSet<_>>();
                if requested
                    .iter()
                    .all(|capability| acknowledged.contains(capability))
                {
                    return Ok(true);
                }
                if required {
                    return Err(Error::CapabilityRejected);
                }
                return Ok(false);
            }
            Command::CAP(_, CapSubCommand::NAK, _, _) => {
                if required {
                    return Err(Error::CapabilityRejected);
                }
                return Ok(false);
            }
            _ => {}
        }
    }
}

async fn receive_registration_message<S>(wire: &mut Framed<S, WireCodec>) -> Result<Message, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = wire.next().await.ok_or(Error::Disconnected)??;
        if let Command::PING(server, second) = message.command {
            wire.send(Command::PONG(server, second).into()).await?;
        } else {
            return Ok(message);
        }
    }
}

fn capability_names<'a>(
    parameter: Option<&'a str>,
    trailing: Option<&'a str>,
) -> impl Iterator<Item = &'a str> {
    trailing
        .or(parameter)
        .into_iter()
        .flat_map(str::split_whitespace)
        .map(|capability| capability.trim_start_matches(['~', '=']))
        .map(|capability| {
            capability
                .split_once('=')
                .map_or(capability, |(name, _value)| name)
        })
}

fn message_tag<'a>(message: &'a Message, name: &str) -> Option<&'a str> {
    message
        .tags
        .iter()
        .flatten()
        .find(|tag| tag.0 == name)
        .and_then(|tag| tag.1.as_deref())
}

const fn is_sasl_failure(response: Response) -> bool {
    matches!(
        response,
        Response::ERR_SASLFAIL
            | Response::ERR_SASLTOOLONG
            | Response::ERR_SASLABORT
            | Response::ERR_SASLALREADY
    )
}

struct WireCodec {
    inner: IrcCodec,
}

impl WireCodec {
    fn new() -> Result<Self, irc_proto::error::ProtocolError> {
        IrcCodec::new("UTF-8").map(|inner| Self { inner })
    }
}

impl Decoder for WireCodec {
    type Error = irc_proto::error::ProtocolError;
    type Item = Message;

    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let line_length = source
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| position + 1);
        if line_length > MAXIMUM_LINE_BYTES {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "IRC line exceeds limit").into(),
            );
        }
        self.inner.decode(source)
    }
}

impl Encoder<Message> for WireCodec {
    type Error = irc_proto::error::ProtocolError;

    fn encode(&mut self, message: Message, destination: &mut BytesMut) -> Result<(), Self::Error> {
        let start = destination.len();
        self.inner.encode(message, destination)?;
        if destination.len().saturating_sub(start) > MAXIMUM_LINE_BYTES {
            destination.truncate(start);
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "IRC line exceeds limit").into(),
            );
        }
        Ok(())
    }
}

/// Failure establishing or using the IRC connection.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The operation exceeded the connection deadline.
    #[error("IRC connection or registration timed out")]
    Timeout,
    /// The endpoint host cannot be used as a TLS server name.
    #[error("invalid IRC TLS server name '{0}'")]
    InvalidServerName(String),
    /// Opening the TCP stream failed.
    #[error("IRC connection I/O error: {0}")]
    Io(#[from] io::Error),
    /// TLS setup or authentication failed.
    #[error("IRC TLS error: {0}")]
    Tls(#[from] TlsError),
    /// IRC framing or parsing failed.
    #[error("IRC protocol error: {0}")]
    Protocol(#[from] irc_proto::error::ProtocolError),
    /// At least one client certificate is required.
    #[error("IRC client certificate chain is empty")]
    MissingClientCertificate,
    /// The local leaf certificate is not the one attested in the manifest.
    #[error("IRC client certificate does not match the manifest attestation")]
    ClientCertificateMismatch,
    /// The selected member has no IRC identity attestation.
    #[error("IRC identity has no manifest attestation for {0}")]
    MissingAttestation(Did),
    /// The configured channel set is unexpectedly empty.
    #[error("IRC connection has no configured channels")]
    MissingChannels,
    /// A required IRCv3 capability was not advertised.
    #[error("IRC server does not advertise required capability '{0}'")]
    MissingCapability(&'static str),
    /// The server rejected the requested IRCv3 capabilities.
    #[error("IRC server rejected required capabilities")]
    CapabilityRejected,
    /// SASL `EXTERNAL` did not authenticate the client certificate.
    #[error("IRC SASL EXTERNAL authentication failed")]
    SaslAuthentication,
    /// The server removed an identity capability after registration.
    #[error("IRC server removed required capability '{0}'")]
    RequiredCapabilityLost(&'static str),
    /// The IRC stream ended.
    #[error("IRC server disconnected")]
    Disconnected,
    /// The server rejected a negotiated history request.
    #[error("IRC history request failed: {0}")]
    History(String),
    /// An outbound target was not provided by the manifest.
    #[error("IRC channel {0} is not bound by the manifest")]
    UnboundChannel(IrcChannel),
    /// Outbound messages must contain at least one nonempty line.
    #[error("IRC message body must contain a nonempty line")]
    InvalidMessageBody,
}

impl Error {
    /// Returns whether reconnecting can plausibly recover without changing trusted configuration.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Io(_)
                | Self::Disconnected
                | Self::Protocol(irc_proto::error::ProtocolError::Io(_))
        )
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{collections::BTreeSet, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use config::{Config as ConfigLoader, File, FileFormat};
    use futures_util::{SinkExt as _, StreamExt as _};
    use irc_proto::{Command, Message};
    use radicle::{
        crypto::{Signature, signature::Signer as _, ssh::keystore::MemorySigner},
        identity::{Did, RepoId},
    };
    use rcgen::{CertifiedKey, PublicKeyData as _, generate_simple_self_signed};
    use rustls::{
        RootCertStore, ServerConfig,
        client::danger::ServerCertVerifier as _,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
        server::WebPkiClientVerifier,
    };
    use sha2::{Digest as _, Sha256};
    use tokio::{io::duplex, net::TcpListener};
    use tokio_rustls::TlsAcceptor;
    use tokio_util::{bytes::BytesMut, codec::Framed};

    use crate::{
        chat::{
            AttestationPayload, ClientCertificateFingerprint, Config, IrcAccount, IrcChannel,
            MessageId, Server, ServerEndpoint, ServerPin, State,
            classification::IncomingMessage,
            runtime::{Delivery, Ingress},
            transport::{
                ClientIdentity, Connection, Error, PinnedServerVerifier, WireCodec, register,
                tls_configuration,
            },
        },
        conversation,
        manifest::{Limits, PriorityLabels, RepositoryPolicy, TriggerAccess},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    fn certificate() -> (CertificateDer<'static>, Vec<u8>, Vec<u8>) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("test certificate should be generated");
        (
            cert.der().clone(),
            signing_key.serialize_der(),
            signing_key.subject_public_key_info(),
        )
    }

    fn identity(certificate: CertificateDer<'static>, private_key: Vec<u8>) -> ClientIdentity {
        ClientIdentity::new(
            vec![certificate],
            PrivatePkcs8KeyDer::from(private_key).into(),
        )
        .expect("test identity should be valid")
    }

    fn verifier(pin: ServerPin) -> PinnedServerVerifier {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        PinnedServerVerifier {
            pin,
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn chat_config(
        server: &Server,
        signer: &MemorySigner,
        certificate: ClientCertificateFingerprint,
    ) -> (Config, Did) {
        let repository = REPOSITORY.parse::<RepoId>().expect("RID should parse");
        let did = Did::from(*radicle::crypto::Signer::public_key(signer));
        let account = "nico".parse::<IrcAccount>().expect("account should parse");
        let payload = AttestationPayload::new(
            repository,
            server.clone(),
            account.clone(),
            certificate,
            did,
        );
        let signature: Signature = signer.sign(payload.canonical().as_bytes());
        let alice = MemorySigner::r#gen();
        let alice_did = Did::from(*radicle::crypto::Signer::public_key(&alice));
        let alice_account = "alice".parse::<IrcAccount>().expect("account should parse");
        let alice_certificate = ClientCertificateFingerprint::from_digest([9; 32]);
        let alice_payload = AttestationPayload::new(
            repository,
            server.clone(),
            alice_account.clone(),
            alice_certificate,
            alice_did,
        );
        let alice_signature: Signature = alice.sign(alice_payload.canonical().as_bytes());
        let document = format!(
            concat!(
                "repository = \"{}\"\n",
                "[server]\n",
                "endpoint = \"{}\"\n",
                "pin = \"{}\"\n",
                "[channels]\n",
                "team = \"#project\"\n",
                "[[participants]]\n",
                "account = \"{}\"\n",
                "certificate = \"{}\"\n",
                "did = \"{}\"\n",
                "signature = \"{}\"\n",
                "[[participants]]\n",
                "account = \"{}\"\n",
                "certificate = \"{}\"\n",
                "did = \"{}\"\n",
                "signature = \"{}\"\n",
            ),
            repository,
            server.endpoint(),
            server.pin(),
            account,
            certificate,
            did,
            signature,
            alice_account,
            alice_certificate,
            alice_did,
            alice_signature,
        );
        let config = ConfigLoader::builder()
            .add_source(File::from_str(&document, FileFormat::Toml))
            .build()
            .expect("chat fixture should load")
            .try_deserialize::<Config>()
            .expect("chat fixture should deserialize")
            .validate(&[did])
            .expect("chat fixture should validate");

        (config, did)
    }

    struct ConnectionFixture {
        listener: TcpListener,
        server_tls: Arc<ServerConfig>,
        config: Config,
        did: Did,
        identity: ClientIdentity,
        project: IrcChannel,
        channels: BTreeSet<IrcChannel>,
    }

    async fn connection_fixture() -> ConnectionFixture {
        let (server_certificate, server_key, server_spki) = certificate();
        let (client_certificate, client_key, _client_spki) = certificate();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut client_roots = RootCertStore::empty();
        client_roots
            .add(client_certificate.clone())
            .expect("client certificate should be a trust anchor");
        let client_verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(client_roots), provider.clone())
                .build()
                .expect("client verifier should build");
        let server_tls = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions should be supported")
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(
                vec![server_certificate],
                PrivatePkcs8KeyDer::from(server_key).into(),
            )
            .expect("server TLS should build");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test IRC server should bind");
        let address = listener
            .local_addr()
            .expect("test IRC address should be available");
        let server = Server::new(
            format!("ircs://127.0.0.1:{}", address.port())
                .parse::<ServerEndpoint>()
                .expect("test endpoint should parse"),
            ServerPin::from_digest(Sha256::digest(server_spki).into()),
        );
        let client_fingerprint = ClientCertificateFingerprint::from_digest(
            Sha256::digest(client_certificate.as_ref()).into(),
        );
        let signer = MemorySigner::r#gen();
        let (config, did) = chat_config(&server, &signer, client_fingerprint);
        let project = "#project"
            .parse::<IrcChannel>()
            .expect("channel should parse");
        let channels = [project.clone()].into_iter().collect::<BTreeSet<_>>();

        ConnectionFixture {
            listener,
            server_tls: Arc::new(server_tls),
            config,
            did,
            identity: identity(client_certificate, client_key),
            project,
            channels,
        }
    }

    #[test]
    fn accepts_only_the_exact_server_spki() {
        let (certificate, _private_key, spki) = certificate();
        let pin = ServerPin::from_digest(Sha256::digest(spki).into());
        let name = ServerName::try_from("localhost").expect("server name should parse");
        let now = UnixTime::since_unix_epoch(Duration::ZERO);

        verifier(pin)
            .verify_server_cert(&certificate, &[], &name, &[], now)
            .expect("matching SPKI should verify");
        let mismatch = ServerPin::from_digest([0; 32]);
        verifier(mismatch)
            .verify_server_cert(&certificate, &[], &name, &[], now)
            .expect_err("another SPKI must be rejected");
    }

    #[test]
    fn requires_the_attested_client_certificate() {
        let (certificate, private_key, _spki) = certificate();
        let fingerprint =
            ClientCertificateFingerprint::from_digest(Sha256::digest(certificate.as_ref()).into());
        let identity = identity(certificate, private_key);

        identity
            .verify_fingerprint(fingerprint)
            .expect("attested certificate should match");
        identity
            .verify_fingerprint(ClientCertificateFingerprint::from_digest([0; 32]))
            .expect_err("another certificate must not authenticate");
    }

    #[test]
    fn builds_tls_with_client_authentication_and_the_custom_verifier() {
        let (certificate, private_key, spki) = certificate();
        let pin = ServerPin::from_digest(Sha256::digest(spki).into());

        tls_configuration(pin, identity(certificate, private_key))
            .expect("pinned client TLS configuration should build");
    }

    #[tokio::test]
    async fn negotiates_required_capabilities_external_sasl_and_channels() {
        let (client, server) = duplex(16 * 1024);
        let mut client = Framed::new(client, WireCodec::new().expect("codec should initialize"));
        let mut server = Framed::new(server, WireCodec::new().expect("codec should initialize"));
        let account = "nico".parse::<IrcAccount>().expect("account should parse");
        let channels = ["#project", "#security"]
            .into_iter()
            .map(|channel| channel.parse::<IrcChannel>().expect("channel should parse"))
            .collect::<BTreeSet<_>>();
        let expected_channels = channels.clone();

        let client = async move { register(&mut client, &account, "nico", &channels).await };
        let server = serve_registration(&mut server, &expected_channels, true);

        let (registered, ()) = tokio::join!(client, server);
        assert!(registered.expect("registration should succeed"));
    }

    #[tokio::test]
    async fn opens_pinned_tls_and_exchanges_only_bound_messages() {
        let ConnectionFixture {
            listener,
            server_tls,
            config,
            did,
            identity,
            project,
            channels,
        } = connection_fixture().await;

        let serving = async move {
            let (tcp, _peer) = listener.accept().await.expect("client should connect");
            let tls = TlsAcceptor::from(server_tls)
                .accept(tcp)
                .await
                .expect("client certificate should authenticate");
            let mut wire = Framed::new(tls, WireCodec::new().expect("codec should initialize"));
            serve_registration(&mut wire, &channels, true).await;
            let history = receive(&mut wire).await;
            assert!(matches!(
                history.command,
                Command::Raw(ref command, ref parameters)
                    if command == "CHATHISTORY"
                        && parameters == &["LATEST", "#project", "*", "1000"]
            ));
            send(
                &mut wire,
                "@account=alice;msgid=live-1 :alice!u@h PRIVMSG #project :@nico live",
            )
            .await;
            send(&mut wire, ":server BATCH +history chathistory #project").await;
            send(
                &mut wire,
                "@batch=history;account=alice;msgid=old-1 :alice!u@h PRIVMSG #project :@nico old",
            )
            .await;
            send(&mut wire, "PING :live").await;
            assert!(
                matches!(receive(&mut wire).await.command, Command::PONG(ref value, None) if value == "live")
            );
            send(&mut wire, ":server BATCH -history").await;
            assert!(matches!(
                receive(&mut wire).await.command,
                Command::PRIVMSG(ref channel, ref body)
                    if channel == "#project" && body == "reply"
            ));
            for expected in ["first", "second"] {
                assert!(matches!(
                    receive(&mut wire).await.command,
                    Command::PRIVMSG(ref channel, ref body)
                        if channel == "#project" && body == expected
                ));
            }
            send(&mut wire, ":server CAP nico DEL :account-tag").await;
        };
        let connecting = async move {
            let mut connection = Connection::connect(&config, Member::Nico, did, identity)
                .await
                .expect("pinned IRC connection should open");
            assert!(connection.has_history());
            let history = connection
                .history(&project)
                .await
                .expect("history should replay");
            assert!(matches!(
                history.as_slice(),
                [Message { command: Command::PRIVMSG(channel, body), .. }]
                    if channel == "#project" && body == "@nico old"
            ));
            assert!(matches!(
                connection.next().await.expect("message should arrive").command,
                Command::PRIVMSG(ref channel, ref body)
                    if channel == "#project" && body == "@nico live"
            ));
            connection
                .send(&project, "reply")
                .await
                .expect("bound reply should send");
            connection
                .send(
                    &"#other"
                        .parse::<IrcChannel>()
                        .expect("other channel should parse"),
                    "reply",
                )
                .await
                .expect_err("unbound channel must be rejected");
            connection
                .send(&project, "first\nsecond")
                .await
                .expect("multiline reply should be split into IRC messages");
            assert!(matches!(
                connection.next().await,
                Err(Error::RequiredCapabilityLost("account-tag"))
            ));
        };

        let ((), ()) = tokio::join!(serving, connecting);
    }

    #[derive(Debug)]
    struct ReplyingDelivery {
        repository: RepoId,
        state: State,
        delay: Duration,
    }

    #[async_trait]
    impl Delivery for ReplyingDelivery {
        async fn deliver(
            &self,
            repository: RepoId,
            identity: MessageId,
            message: IncomingMessage,
        ) -> Result<conversation::Acknowledgement, String> {
            if repository != self.repository {
                return Err("delivery reached another repository".to_owned());
            }
            tokio::time::sleep(self.delay).await;
            self.state
                .enqueue_reply(repository, &message.channel, identity, "reply".to_owned())
                .await
                .map(|_reply| conversation::Acknowledgement::completed())
                .map_err(|error| error.to_string())
        }
    }

    #[tokio::test]
    async fn reconnects_after_a_transient_disconnect() {
        let ConnectionFixture {
            listener,
            server_tls,
            config,
            did,
            identity,
            channels,
            ..
        } = connection_fixture().await;
        let repository = REPOSITORY.parse::<RepoId>().expect("RID should parse");
        let policy = RepositoryPolicy::new(
            TriggerAccess::Anyone,
            vec![did],
            PriorityLabels::default(),
            Limits::default(),
        )
        .with_chat_for_test(config);
        let state = State::memory().await.expect("chat state should open");
        let ingress = Ingress::for_test(
            repository,
            Member::Nico,
            did,
            policy,
            vec![did],
            identity,
            state.clone(),
            ReplyingDelivery {
                repository,
                state,
                delay: Duration::from_millis(100),
            },
        );

        let serving = async move {
            for attempt in 0..2 {
                let (tcp, _peer) = listener.accept().await.expect("client should connect");
                let tls = TlsAcceptor::from(server_tls.clone())
                    .accept(tcp)
                    .await
                    .expect("client certificate should authenticate");
                let mut wire = Framed::new(tls, WireCodec::new().expect("codec should initialize"));
                serve_registration(&mut wire, &channels, false).await;
                if attempt == 1 {
                    send(
                        &mut wire,
                        "@account=alice;msgid=reconnected-1 :alice!u@h PRIVMSG #project :@nico \
                         hello",
                    )
                    .await;
                    send(&mut wire, "PING :during-delivery").await;
                    assert!(matches!(
                        receive(&mut wire).await.command,
                        Command::PONG(ref server, None) if server == "during-delivery"
                    ));
                    assert!(matches!(
                        receive(&mut wire).await.command,
                        Command::PRIVMSG(ref channel, ref body)
                            if channel == "#project" && body == "reply"
                    ));
                    send(&mut wire, ":server CAP nico DEL :account-tag").await;
                }
            }
        };
        let reconnecting = ingress.run();
        let ((), result) = tokio::join!(serving, reconnecting);

        assert!(matches!(
            result,
            Err(crate::chat::runtime::Error::Transport(
                Error::RequiredCapabilityLost("account-tag")
            ))
        ));
    }

    #[tokio::test]
    async fn rejects_missing_identity_capabilities() {
        let (client, server) = duplex(4_096);
        let mut client = Framed::new(client, WireCodec::new().expect("codec should initialize"));
        let mut server = Framed::new(server, WireCodec::new().expect("codec should initialize"));
        let account = "nico".parse::<IrcAccount>().expect("account should parse");
        let channels = ["#project"
            .parse::<IrcChannel>()
            .expect("channel should parse")]
        .into_iter()
        .collect();

        let client = async move { register(&mut client, &account, "nico", &channels).await };
        let server = async move {
            for _ in 0..3 {
                let _message = receive(&mut server).await;
            }
            send(&mut server, ":server CAP * LS :sasl").await;
        };
        let (result, ()) = tokio::join!(client, server);

        assert!(matches!(
            result,
            Err(Error::MissingCapability("account-tag"))
        ));
    }

    #[tokio::test]
    async fn rejected_history_capability_degrades() {
        let (client, server) = duplex(4_096);
        let mut client = Framed::new(client, WireCodec::new().expect("codec should initialize"));
        let mut server = Framed::new(server, WireCodec::new().expect("codec should initialize"));

        let receiving = super::receive_capability_acknowledgement(
            &mut client,
            &["draft/chathistory", "batch"],
            false,
        );
        let rejecting = send(&mut server, ":server CAP * NAK :draft/chathistory batch");
        let (result, ()) = tokio::join!(receiving, rejecting);

        assert!(!result.expect("optional rejection should not fail the connection"));
    }

    #[test]
    fn rejects_oversized_irc_lines() {
        use tokio_util::codec::Decoder as _;

        let mut codec = WireCodec::new().expect("codec should initialize");
        let mut line = BytesMut::from(&vec![b'a'; super::MAXIMUM_LINE_BYTES + 1][..]);

        codec
            .decode(&mut line)
            .expect_err("unbounded IRC input must be rejected");
    }

    #[test]
    fn splits_large_unicode_replies_below_the_wire_limit() {
        let body = "é".repeat(10_000);
        let parts = super::reply_parts(&body).expect("large reply should split");

        assert!(
            parts
                .iter()
                .all(|part| !part.is_empty() && part.len() <= super::MAXIMUM_REPLY_BODY_BYTES)
        );
        assert_eq!(parts.concat(), body);
    }

    async fn serve_registration<S>(
        wire: &mut Framed<S, WireCodec>,
        expected_channels: &BTreeSet<IrcChannel>,
        history: bool,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        assert!(matches!(receive(wire).await.command, Command::CAP(..)));
        assert!(matches!(receive(wire).await.command, Command::NICK(ref nick) if nick == "nico"));
        assert!(matches!(receive(wire).await.command, Command::USER(..)));
        send(wire, "PING :registration").await;
        assert!(
            matches!(receive(wire).await.command, Command::PONG(ref value, None) if value == "registration")
        );
        let capabilities = if history {
            ":server CAP * LS :sasl=EXTERNAL account-tag message-tags batch draft/chathistory"
        } else {
            ":server CAP * LS :sasl=EXTERNAL account-tag"
        };
        send(wire, capabilities).await;
        let request = receive(wire).await;
        assert!(matches!(
            &request.command,
            Command::CAP(_, irc_proto::CapSubCommand::REQ, _, _)
        ));
        if let Command::CAP(_, _, parameter, trailing) = request.command {
            let capabilities = super::capability_names(parameter.as_deref(), trailing.as_deref())
                .collect::<BTreeSet<_>>();
            assert!(capabilities.contains("sasl"));
            assert!(capabilities.contains("account-tag"));
            assert_eq!(capabilities.contains("message-tags"), history);
        }
        let acknowledgement = if history {
            ":server CAP * ACK :sasl account-tag message-tags"
        } else {
            ":server CAP * ACK :sasl account-tag"
        };
        send(wire, acknowledgement).await;
        if history {
            let history_request = receive(wire).await;
            if let Command::CAP(_, _, parameter, trailing) = history_request.command {
                let capabilities =
                    super::capability_names(parameter.as_deref(), trailing.as_deref())
                        .collect::<BTreeSet<_>>();
                assert!(capabilities.contains("draft/chathistory"));
                assert!(capabilities.contains("batch"));
            }
            send(wire, ":server CAP * ACK :batch draft/chathistory").await;
        }
        assert!(
            matches!(receive(wire).await.command, Command::AUTHENTICATE(ref mechanism) if mechanism == "EXTERNAL")
        );
        send(wire, "AUTHENTICATE +").await;
        assert!(
            matches!(receive(wire).await.command, Command::AUTHENTICATE(ref response) if response == "bmljbw==")
        );
        send(wire, ":server 903 nico :SASL authentication successful").await;
        assert!(matches!(
            receive(wire).await.command,
            Command::CAP(_, irc_proto::CapSubCommand::END, _, _)
        ));
        send(wire, ":server 001 nico :Welcome").await;
        let mut joined = BTreeSet::new();
        for _ in 0..expected_channels.len() {
            if let Command::JOIN(channel, None, None) = receive(wire).await.command {
                let _inserted = joined.insert(
                    channel
                        .parse::<IrcChannel>()
                        .expect("joined channel should parse"),
                );
            }
        }
        assert_eq!(&joined, expected_channels);
    }

    async fn receive<S>(wire: &mut Framed<S, WireCodec>) -> Message
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        wire.next()
            .await
            .expect("peer should send a message")
            .expect("message should decode")
    }

    async fn send<S>(wire: &mut Framed<S, WireCodec>, message: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        wire.send(
            message
                .parse::<Message>()
                .expect("fixture message should parse"),
        )
        .await
        .expect("fixture message should send");
    }
}
