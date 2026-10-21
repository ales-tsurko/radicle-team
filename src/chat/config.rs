//! Repository chat configuration and Radicle-signed IRC identity attestations.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use radicle::{
    crypto::{PublicKey, Signature, VerifyingKey, signature::Verifier as _},
    identity::{Did, RepoId},
};
use serde::{Deserialize, Deserializer, de};
use url::{Host, Url};

use crate::chat::{AuthenticatedSender, ChannelId};

const PAYLOAD_DOMAIN: &str = "radicle-team-chat-attestation-v1";
const SHA256_BYTES: usize = 32;

/// IRC configuration authorized by one repository manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    repository: RepoId,
    server: Server,
    channels: BTreeMap<ChannelId, IrcChannel>,
    participants: Vec<Attestation>,
}

impl Config {
    /// Returns the repository named by every identity attestation.
    #[must_use]
    pub const fn repository(&self) -> RepoId {
        self.repository
    }

    /// Returns the pinned IRC server.
    #[must_use]
    pub const fn server(&self) -> &Server {
        &self.server
    }

    /// Returns the configured logical-to-IRC channel mappings.
    pub fn channels(&self) -> impl Iterator<Item = (&ChannelId, &IrcChannel)> {
        self.channels.iter()
    }

    /// Returns the IRC channel bound to one repository-local channel.
    pub(crate) fn irc_channel(&self, channel: &ChannelId) -> Option<&IrcChannel> {
        self.channels.get(channel)
    }

    /// Returns the attestation for `did`, when one exists.
    #[must_use]
    pub fn participant(&self, did: Did) -> Option<&Attestation> {
        self.participants
            .iter()
            .find(|participant| participant.did == did)
    }

    pub(crate) fn validate(self, members: &[Did]) -> Result<Self, String> {
        self.validate_inner(members)
            .map_err(|error| error.to_string())
    }

    fn validate_inner(self, members: &[Did]) -> Result<Self, Error> {
        if self.channels.is_empty() {
            return Err(Error::MissingChannels);
        }
        if self.participants.is_empty() {
            return Err(Error::MissingParticipants);
        }

        let mut irc_channels = BTreeSet::new();
        for channel in self.channels.values() {
            if !irc_channels.insert(channel.clone()) {
                return Err(Error::DuplicateChannel(channel.clone()));
            }
        }

        let mut accounts = BTreeSet::new();
        let mut certificates = BTreeSet::new();
        let mut dids = BTreeSet::new();
        for participant in &self.participants {
            if !accounts.insert(participant.account.clone()) {
                return Err(Error::DuplicateAccount(participant.account.clone()));
            }
            if !certificates.insert(participant.certificate) {
                return Err(Error::DuplicateCertificate(participant.certificate));
            }
            if !dids.insert(participant.did) {
                return Err(Error::DuplicateDid(participant.did));
            }
            participant.verify(self.repository, &self.server)?;
        }
        for member in members {
            if !dids.contains(member) {
                return Err(Error::MissingMember(*member));
            }
        }

        Ok(self)
    }

    pub(crate) fn authenticate(
        &self,
        repository: RepoId,
        server: ServerPin,
        account: &IrcAccount,
    ) -> Result<AuthenticatedSender, AuthenticationError> {
        if repository != self.repository {
            return Err(AuthenticationError::Repository);
        }
        if server != self.server.pin {
            return Err(AuthenticationError::Server);
        }
        self.participants
            .iter()
            .find(|participant| &participant.account == account)
            .map(|participant| AuthenticatedSender::new(participant.did))
            .ok_or(AuthenticationError::Account)
    }
}

/// One TLS IRC endpoint and its pinned public key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    endpoint: ServerEndpoint,
    pin: ServerPin,
}

impl Server {
    /// Creates a pinned IRC server description.
    #[must_use]
    pub const fn new(endpoint: ServerEndpoint, pin: ServerPin) -> Self {
        Self { endpoint, pin }
    }

    /// Returns the canonical TLS endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> &ServerEndpoint {
        &self.endpoint
    }

    /// Returns the pinned server public-key fingerprint.
    #[must_use]
    pub const fn pin(&self) -> ServerPin {
        self.pin
    }
}

/// Canonical `ircs://host:port` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerEndpoint {
    canonical: String,
    host: String,
    port: u16,
}

impl ServerEndpoint {
    /// Returns the canonical endpoint string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Returns the host without IPv6 URL brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the explicit TLS port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for ServerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.canonical.fmt(formatter)
    }
}

impl FromStr for ServerEndpoint {
    type Err = InvalidServerEndpoint;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_error| InvalidServerEndpoint(value.to_owned()))?;
        if url.scheme() != "ircs"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(InvalidServerEndpoint(value.to_owned()));
        }
        let port = url
            .port()
            .ok_or_else(|| InvalidServerEndpoint(value.to_owned()))?;
        let (host, url_host) = match url
            .host()
            .ok_or_else(|| InvalidServerEndpoint(value.to_owned()))?
        {
            Host::Domain(host) => {
                let host = host.to_ascii_lowercase();
                (host.clone(), host)
            }
            Host::Ipv4(host) => {
                let host = host.to_string();
                (host.clone(), host)
            }
            Host::Ipv6(host) => (host.to_string(), format!("[{host}]")),
        };
        let canonical = format!("ircs://{url_host}:{port}");
        if value != canonical {
            return Err(InvalidServerEndpoint(value.to_owned()));
        }

        Ok(Self {
            canonical,
            host,
            port,
        })
    }
}

impl<'de> Deserialize<'de> for ServerEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// An IRC TLS endpoint is not canonical or safe.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid IRC TLS endpoint '{0}'; expected canonical ircs://host:port")]
pub struct InvalidServerEndpoint(String);

/// SHA-256 pin of the server certificate's DER-encoded subject public-key information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerPin([u8; SHA256_BYTES]);

impl FromStr for ServerPin {
    type Err = InvalidServerPin;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value, "spki-sha256:")
            .map(Self)
            .map_err(|()| InvalidServerPin(value.to_owned()))
    }
}

impl ServerPin {
    /// Creates a pin from the SHA-256 digest of DER-encoded SubjectPublicKeyInfo.
    #[must_use]
    pub const fn from_digest(digest: [u8; SHA256_BYTES]) -> Self {
        Self(digest)
    }

    /// Returns the raw SHA-256 digest.
    #[must_use]
    pub const fn digest(self) -> [u8; SHA256_BYTES] {
        self.0
    }
}

impl fmt::Display for ServerPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_digest(formatter, "spki-sha256:", &self.0)
    }
}

impl<'de> Deserialize<'de> for ServerPin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A server public-key pin is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid server pin '{0}'; expected spki-sha256: followed by 64 lowercase hex digits")]
pub struct InvalidServerPin(String);

/// SHA-256 fingerprint of one complete DER-encoded IRC client certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientCertificateFingerprint([u8; SHA256_BYTES]);

impl FromStr for ClientCertificateFingerprint {
    type Err = InvalidClientCertificateFingerprint;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value, "cert-sha256:")
            .map(Self)
            .map_err(|()| InvalidClientCertificateFingerprint(value.to_owned()))
    }
}

impl ClientCertificateFingerprint {
    /// Creates a fingerprint from the SHA-256 digest of a DER-encoded client certificate.
    #[must_use]
    pub const fn from_digest(digest: [u8; SHA256_BYTES]) -> Self {
        Self(digest)
    }

    /// Returns the raw SHA-256 digest.
    #[must_use]
    pub const fn digest(self) -> [u8; SHA256_BYTES] {
        self.0
    }
}

impl fmt::Display for ClientCertificateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_digest(formatter, "cert-sha256:", &self.0)
    }
}

impl<'de> Deserialize<'de> for ClientCertificateFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A client certificate fingerprint is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid client certificate fingerprint '{0}'; expected cert-sha256: followed by 64 lowercase \
     hex digits"
)]
pub struct InvalidClientCertificateFingerprint(String);

/// Canonical lowercase IRC account name used for authorization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IrcAccount(String);

impl IrcAccount {
    /// Returns the account name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IrcAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for IrcAccount {
    type Err = InvalidIrcAccount;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if is_portable_name(value, 64) {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidIrcAccount(value.to_owned()))
        }
    }
}

impl<'de> Deserialize<'de> for IrcAccount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// An IRC account name is outside the portable canonical subset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid IRC account '{0}'; expected 1-64 lowercase ASCII account characters")]
pub struct InvalidIrcAccount(String);

/// Canonical IRC channel name bound to a logical repository channel.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IrcChannel(String);

impl IrcChannel {
    /// Returns the channel name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IrcChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for IrcChannel {
    type Err = InvalidIrcChannel;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value
            .strip_prefix('#')
            .is_some_and(|name| is_portable_name(name, 63))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidIrcChannel(value.to_owned()))
        }
    }
}

impl<'de> Deserialize<'de> for IrcChannel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// An IRC channel name is outside the portable canonical subset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid IRC channel '{0}'; expected # followed by lowercase ASCII channel characters")]
pub struct InvalidIrcChannel(String);

fn is_portable_name(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_' | '.')
        })
}

/// One Radicle-signed binding from an IRC account and client certificate to a DID.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    account: IrcAccount,
    certificate: ClientCertificateFingerprint,
    did: Did,
    #[serde(deserialize_with = "radicle::serde_ext::string::deserialize")]
    signature: Signature,
}

impl Attestation {
    /// Returns the authenticated IRC account.
    #[must_use]
    pub const fn account(&self) -> &IrcAccount {
        &self.account
    }

    /// Returns the enrolled IRC client certificate fingerprint.
    #[must_use]
    pub const fn certificate(&self) -> ClientCertificateFingerprint {
        self.certificate
    }

    /// Returns the Radicle DID which signed this binding.
    #[must_use]
    pub const fn did(&self) -> Did {
        self.did
    }

    fn verify(&self, repository: RepoId, server: &Server) -> Result<(), Error> {
        let payload = AttestationPayload::new(
            repository,
            server.clone(),
            self.account.clone(),
            self.certificate,
            self.did,
        );
        let public_key = PublicKey::from(self.did);
        VerifyingKey::try_from(&public_key)
            .and_then(|key| key.verify(payload.canonical().as_bytes(), &self.signature))
            .map_err(|_error| Error::InvalidSignature {
                account: self.account.clone(),
                did: self.did,
            })
    }
}

/// Fields covered by one canonical chat identity signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationPayload {
    repository: RepoId,
    server: Server,
    account: IrcAccount,
    certificate: ClientCertificateFingerprint,
    did: Did,
}

impl AttestationPayload {
    /// Creates an attestation payload from validated canonical fields.
    #[must_use]
    pub const fn new(
        repository: RepoId,
        server: Server,
        account: IrcAccount,
        certificate: ClientCertificateFingerprint,
        did: Did,
    ) -> Self {
        Self {
            repository,
            server,
            account,
            certificate,
            did,
        }
    }

    /// Returns the exact UTF-8 text signed by the Radicle key.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            concat!(
                "{}\n",
                "repository={}\n",
                "server={}\n",
                "server-pin={}\n",
                "account={}\n",
                "client-certificate={}\n",
                "did={}\n",
            ),
            PAYLOAD_DOMAIN,
            self.repository,
            self.server.endpoint,
            self.server.pin,
            self.account,
            self.certificate,
            self.did,
        )
    }
}

fn parse_digest(value: &str, prefix: &str) -> Result<[u8; SHA256_BYTES], ()> {
    let value = value.strip_prefix(prefix).ok_or(())?;
    if value.len() != SHA256_BYTES * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(());
    }
    let mut bytes = [0_u8; SHA256_BYTES];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = hex_digit(pair[0]).ok_or(())?;
        let low = hex_digit(pair[1]).ok_or(())?;
        let byte = bytes.get_mut(index).ok_or(())?;
        *byte = high * 16 + low;
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn write_digest(
    formatter: &mut fmt::Formatter<'_>,
    prefix: &str,
    bytes: &[u8; SHA256_BYTES],
) -> fmt::Result {
    formatter.write_str(prefix)?;
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum Error {
    #[error("chat configuration must bind at least one channel")]
    MissingChannels,
    #[error("chat configuration must attest at least one participant")]
    MissingParticipants,
    #[error("IRC channel {0} is bound more than once")]
    DuplicateChannel(IrcChannel),
    #[error("IRC account {0} is attested more than once")]
    DuplicateAccount(IrcAccount),
    #[error("IRC client certificate {0} is attested more than once")]
    DuplicateCertificate(ClientCertificateFingerprint),
    #[error("Radicle identity {0} is attested more than once")]
    DuplicateDid(Did),
    #[error("active member identity {0} has no IRC attestation")]
    MissingMember(Did),
    #[error("IRC account {account} has an invalid attestation signature for {did}")]
    InvalidSignature { account: IrcAccount, did: Did },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AuthenticationError {
    #[error("chat channel belongs to another repository")]
    Repository,
    #[error("chat message did not arrive over the manifest-pinned server")]
    Server,
    #[error("IRC account is not attested by the repository manifest")]
    Account,
}

#[cfg(test)]
mod tests {
    use radicle::{
        crypto::{
            Signature, SigningKey,
            signature::{Signer as _, Verifier as _},
        },
        identity::{Did, RepoId},
    };

    use crate::chat::{
        AttestationPayload, ClientCertificateFingerprint, IrcAccount, Server, ServerEndpoint,
        ServerPin,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const SERVER_PIN: &str =
        "spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CLIENT_CERTIFICATE: &str =
        "cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn payload(signer: &SigningKey) -> AttestationPayload {
        AttestationPayload::new(
            REPOSITORY.parse::<RepoId>().expect("RID should parse"),
            Server::new(
                "ircs://irc.example.com:6697"
                    .parse::<ServerEndpoint>()
                    .expect("server should parse"),
                SERVER_PIN.parse::<ServerPin>().expect("pin should parse"),
            ),
            "nico-project"
                .parse::<IrcAccount>()
                .expect("account should parse"),
            CLIENT_CERTIFICATE
                .parse::<ClientCertificateFingerprint>()
                .expect("certificate should parse"),
            Did::from(*radicle::crypto::Signer::public_key(signer)),
        )
    }

    #[test]
    fn defines_an_exact_canonical_attestation_payload() {
        let signer = SigningKey::from_seed(radicle::crypto::Seed::new(rand::random()));
        let payload = payload(&signer);

        assert_eq!(
            payload.canonical(),
            format!(
                concat!(
                    "radicle-team-chat-attestation-v1\n",
                    "repository={}\n",
                    "server=ircs://irc.example.com:6697\n",
                    "server-pin={}\n",
                    "account=nico-project\n",
                    "client-certificate={}\n",
                    "did={}\n",
                ),
                REPOSITORY,
                SERVER_PIN,
                CLIENT_CERTIFICATE,
                Did::from(*radicle::crypto::Signer::public_key(&signer))
            )
        );
        let signature: Signature = signer.sign(payload.canonical().as_bytes());
        radicle::crypto::VerifyingKey::try_from(&radicle::crypto::PublicKey::from(payload.did))
            .expect("valid verification key")
            .verify(payload.canonical().as_bytes(), &signature)
            .expect("canonical payload signature should verify");
    }

    #[test]
    fn requires_canonical_endpoints_names_and_fingerprints() {
        for endpoint in [
            "irc://irc.example.com:6697",
            "ircs://IRC.example.com:6697",
            "ircs://irc.example.com",
            "ircs://user@irc.example.com:6697",
            "ircs://irc.example.com:6697/path",
        ] {
            endpoint
                .parse::<ServerEndpoint>()
                .expect_err("endpoint should be rejected");
        }
        for account in ["", "Nico", "nico project"] {
            account
                .parse::<IrcAccount>()
                .expect_err("account should be rejected");
        }
        SERVER_PIN
            .to_uppercase()
            .parse::<ServerPin>()
            .expect_err("non-canonical pin should be rejected");
        CLIENT_CERTIFICATE
            .replace("cert-sha256:", "sha256:")
            .parse::<ClientCertificateFingerprint>()
            .expect_err("wrong fingerprint kind should be rejected");

        let endpoint = "ircs://[::1]:6697"
            .parse::<ServerEndpoint>()
            .expect("IPv6 endpoint should parse");
        assert_eq!(endpoint.as_str(), "ircs://[::1]:6697");
        assert_eq!(endpoint.host(), "::1");
        assert_eq!(endpoint.port(), 6697);
    }
}
