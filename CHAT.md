# Chat

People working with a team need a way to reach its members quickly, without
opening an issue for every question. Chat is that channel.

This document records the design, wire-independent formats, and reasoning.

`radicle-team` speaks IRC. A member joins its team's channels as an ordinary
IRC client under its own nick, and the people it works with join as ordinary
users. There is no bot, no gateway, and no platform integration. The team runs
its own server.




## How it works

Each member opens one outbound TLS connection to the team's IRC server and
joins the channels bound to its repository. Members run on separate machines,
so each connects on its own. Nothing listens for inbound connections and no
member needs a public address.

To everyone in the channel a member looks like any other user. Addressing one
by nick reaches it. Two members in the same channel are two users, the same way
two people are.

Nothing about the chosen server appears in the source. `radicle-team` speaks
IRC and IRCv3, so any conforming server works.




## Server requirements

Three capabilities are required and one is optional.

- **TLS** for the connection.
- **SASL** for authentication, preferring `EXTERNAL` with a client certificate.
- **`account-tag`** so every message carries its sender's account.

Capabilities are negotiated at connect. A server without `chathistory` costs
backlog after downtime and nothing else, so it degrades.

`account-tag` does not degrade. Without it a member cannot tell who is talking,
and falling back to nicks would let anyone claim any identity. A member that
does not get it refuses to act on chat rather than guessing.




## Identity and authorization

Authorization keys on the SASL account, never the nick. Nicks are transient and
reassignable. Accounts are what the server authenticated.

The repository manifest carries a signed attestation per participant, covering
both members and the people authorized to direct them. Each attestation names
the RID, the server identity, the IRC account, the client certificate
fingerprint, and the DID. Any member can then verify that an account belongs to
a DID without asking anyone.

At runtime the check is `(pinned server, account)` to DID. The certificate
fingerprint is enrollment evidence held by the server, not runtime identity.
Peers never see another client's fingerprint.

IRC client certificates are generated separately and never derived from the
Radicle key. The same key material should not serve two protocols with
different security arguments. The Radicle key signs the attestation instead,
which gives the binding without the reuse.

Authorization itself stays in the member. Manifest policy and delegate checks
are enforced by the agent, exactly as they are for repository activity. Chat
asks and the agent decides.




## What chat trusts

The IRC server asserts accounts. If it lies, `account-tag` lies, and everything
built on the account-to-DID mapping lies with it. The team runs the server, so
this is trust in its own infrastructure rather than in a third party, but it is
real and it is worth stating rather than discovering later.

The alternative is signing each message inline with the sender's Radicle key.
That needs a Radicle-aware client and would end the ability to use the client
you already have, which is the entire reason for choosing an existing protocol.
The trade is deliberate.




## Channels

A channel binds to one repository, and one repository only. The binding lives
in the manifest as a logical channel identifier mapped to a pinned server and
IRC channel, so it is authorized by the same reviewed, signed file as
everything else. No separate linking or approval exchange is needed.

The channel topic carries the full RID. This catches a channel accidentally
reused across repositories. Topics are mutable by channel operators, so it is a
misconfiguration check and never a security control.

Conversation limits apply per channel and per peer, the same rule already used
for issues and patches. Only an authorized person addressing the member resets
them.

The manifest is readable by anyone who can read the repository, so binding a
channel there publishes where the team talks. The server requires SASL and
channels can be invite-only, so this is acceptable in the ordinary case. Where
it is not, the endpoint belongs in the administrator-owned manifest instead of
the repository.




## Manifest format

Chat is optional. When present, it names the repository again so every signed
identity binding is repository-specific:

```toml
[chat]
repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"

[chat.server]
endpoint = "ircs://irc.example.com:6697"
pin = "spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[chat.channels]
team = "#project"
security = "#project-security"

[[chat.participants]]
account = "nico"
certificate = "cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
did = "did:key:z6Mk..."
signature = "z..."
```

The left side of `[chat.channels]` is the logical `ChannelId`. The right side is
the IRC channel. Account names and IRC channels use a lowercase portable subset
so matching never depends on a server's casemapping. Every active member needs
an attestation. Additional attestations map human accounts, and repository
policy still decides whether their DIDs may act.

`spki-sha256:` is followed by exactly 64 lowercase hexadecimal digits: SHA-256
of the server certificate's DER-encoded SubjectPublicKeyInfo. This survives a
certificate renewal that keeps the key. `cert-sha256:` has the same encoding
but hashes the complete DER-encoded client certificate. The latter is enrollment
evidence, not something IRC peers inspect at runtime.

The signature is the Radicle Ed25519 signature in its normal multibase form. It
covers this exact UTF-8 payload, including the final LF:

```text
radicle-team-chat-attestation-v1
repository=rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc
server=ircs://irc.example.com:6697
server-pin=spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
account=nico
client-certificate=cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
did=did:key:z6Mk...
```

Each DID signs its own binding. Manifest review authorizes the participant, but
the self-signature separately proves that the DID owner accepted this IRC
account and certificate. A rogue or compromised delegate therefore cannot bind
Mallory's IRC account to Alice's DID and inherit Alice's permissions merely by
editing the manifest.

Parsing verifies every signature, rejects duplicate accounts, DIDs,
certificates and IRC channel targets, and requires an attestation for every
active member. Runtime authentication additionally requires the active RID and
observed server pin to match before an `account-tag` can produce a sender DID.




## Privacy

Require SASL to connect and disable open registration, so only accounts the
operator created can open a session at all. Use invite-only, registered-only,
and secret channel modes on top. History follows channel access, so a channel
you are not in does not replay to you.

There is no end-to-end encryption. Whoever runs the server can read everything
and history sits in plaintext at rest. Set a retention limit and document this
for the people using it.




## Deployment

The team deploys the server alongside its members, the same way it already
deploys the members themselves.

[Ergo](https://github.com/ergochat/ergo) is the recommended server. It provides
accounts, SASL, TLS client certificates, history, and bouncer behavior in one
binary. It is a process to deploy rather than a crate to link, so its
implementation language is not a dependency of this project.

Account registration is a one-time operator step, documented rather than
automated. Services syntax differs between IRC servers and automating it would
put server-specific behavior in the source.

[Sable](https://github.com/Libera-Chat/sable) is the Rust server to revisit.
Today it is experimental, its capability set is undocumented, and Libera Chat
does not run it in production. If it conforms, users change servers and this
project does not change.




## Clients

No interface is built here. People use an existing IRC client.

- [Halloy](https://github.com/squidowl/halloy), a desktop client in Rust
- WeeChat and irssi in the terminal
- Goguma on Android and Palaver on iOS
- Gamja, if a browser client is wanted

Mobile works because the server keeps clients always-on and replays history on
reconnect.




## Implementation order

Chat is a second ingress alongside the watcher. Both feed one dispatcher that
owns the talking adapter, its workspace, and its MCP scope, so only one
conversation runs at a time. The building adapter continues to run in parallel.

1. **Invert the watcher.** It currently takes the talking adapter by exclusive
   reference and holds it for its whole run, so a second ingress cannot borrow
   it. Change it to emit deliveries to the dispatcher and wait for
   acknowledgement, checkpointing only after handling and reconciliation
   succeed. Existing watcher test scenarios and assertions stay as they are.
   Mechanical fixture changes are expected, because the interface loses a
   parameter.
2. **Add chat types.** A chat context and its own database key, leaving the
   git-shaped delivery key untouched. Extend `WorkId` so chat-triggered
   repository writes are journalled and traceable back to the message that
   caused them. Add `Talker::messaged`, returning success or failure like every
   other method.
3. **Add a `reply_chat` MCP tool** under the manifest allowlist. It takes text,
   targets the active channel implicitly, and writes to a durable outbox. This
   allows several replies without granting access to arbitrary channels.
4. **Add manifest configuration and identity validation.**
5. **Use `irc-proto` with an application-owned Tokio/Rustls transport.** The
   spike proved that `irc::Client` exposes the required IRCv3 commands and tags,
   but cannot enforce the manifest's exact SPKI pin. In `irc` 1.1.0
   [`src/client/conn.rs:336-381`](https://docs.rs/crate/irc/1.1.0/source/src/client/conn.rs),
   the crate privately constructs its TLS configuration with WebPKI and native
   roots, optionally adds another root, and exposes only an accept-all
   alternative. There is no custom verifier or transport injection point.
   `irc-proto::IrcCodec` preserves `account` and `msgid` tags over a
   caller-owned stream, leaving DNS, TCP, client certificates, and strict SPKI
   verification under this application's control.
6. **Implement connection, classification, replay, and delivery.**

Repository-wide MCP scope is created only after the whole chain succeeds:
message tag to attestation to DID to policy. If any link weakens, chat fails
closed rather than falling back to narrower access. The scope is safe because
only attested accounts can address a member at all, unlike issue text, which
anyone can write. That dependency is the reason for the width, so the two move
together.

Cross-repository isolation is an existing property of the application. Chat
does not add its own.




## Why IRC

An earlier design proposed one bot per chat platform with a gateway routing
messages to members. It was dropped for several reasons.

It requires writing and deploying a router, which is what an IRC server already
is. It requires a per-platform adapter and tracking each platform's API
indefinitely. Chat identity would be a platform account rather than a Radicle
key, because an ordinary Telegram client cannot sign with a DID. A third party
would read every message. Whoever held the gateway could impersonate anyone in
the room.

Running one bot per member instead of a gateway does not survive contact with
the platforms. Telegram caps an account at twenty bots, which members times
repositories reaches at five members and four repositories. Telegram usernames
share one global namespace with all users and channels. Telegram bots cannot
see other bots' messages at all, so members could not share channel context.
None of it removes the platform from the trust path.

Building a chat application is out of scope. Real people talk in these channels
alongside the members and are the same kind of participant, which makes this
group chat: membership, shared history, offline delivery, several clients, and
mobile. That is a product rather than a feature.

The original objections to IRC were all about connecting to arbitrary
third-party servers, where nicks can be claimed while their owner is offline,
accounts must be provisioned on somebody else's machine, and the server cannot
verify Radicle keys. None of that applies to a server the team runs.

Matrix was reconsidered and rejected on weight. Its account and homeserver
objections do dissolve when you own the homeserver, and a Rust homeserver and
client library both exist, but state resolution, device management, and
encryption key handling are far more protocol than this needs.

Nostr remains rejected. Relay-based groups and MLS encrypted messaging are both
draft specifications, private group clients are few, and its keys are a
separate identity from the Radicle one in any case.

XMPP was never considered in the original design and is a real alternative. It
is lighter than Matrix with better mobile support than IRC, and its Rust
libraries are maintained. It loses to IRC on simplicity and has no Rust server
either.

Backing chat with collaborative objects is appealing and was examined closely.
It would delete the whole trust problem, since a message would be signed by the
DID that wrote it, removing the attestation format, the account mapping, and any
reliance on a server asserting identities. It would also reuse the watcher,
classification, and conversation limits rather than running a second ingress.

It fails on visibility. Radicle access control is repository-level only, either
public or private with an allowed set of DIDs, and there is no per-object or
per-type setting. Chat privacy would therefore equal repository visibility, so
on a public repository every message a team ever sent would be world-readable
and permanently archived. Fetching selectively does not help, because that lets
a reader opt out rather than letting a writer restrict access. Gossip latency
and the absence of any existing client are further costs on top.

Redaction is not a way out. Redacting sets the comment to `None` in the
materialized state and appends a new entry, leaving the original text in the
object's history where plain git still reads it. Editing behaves the same way. A
collaborative object is an append-only signed graph, so removing an entry would
break the signature chain and every replica. Peers who already fetched a message
keep it regardless.

SimpleX and building on Radicle gossip, Iroh, or libp2p remain rejected for the
reasons given when they were first considered.
