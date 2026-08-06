# Chat

People working with a team need a way to reach its members quickly, without
opening an issue for every question. Chat is that channel.

`radicle-team` speaks IRC. A member joins its team's channels as an ordinary
IRC client under its own nick, and the people it works with join as ordinary
users. There is no bot, no gateway, and no platform integration. The team runs
its own server.

This document covers what a server must provide, how identity works, and how to
deploy one. The `[chat]` schema is in
[the manifest reference](manifest.md#chat), and the reasoning behind choosing
IRC is in [the architecture notes](architecture.md#rejected-irc-alternatives).




## What chat costs

A member answering chat runs a third agent adapter beside the talking and
building ones, with its own MCP server and its own checked-out workspace. When a
message arrives while the member is reviewing a patch and building another, all
three are live at once.

That is the price of an answer arriving now. An agent turn cannot be
interrupted, so a shared adapter would make every question wait for whatever
review happened to start first.

Omitting `[chat]` from the manifest costs nothing. Neither the adapter nor its
MCP server is started, and the member runs two.




## Server requirements

Three capabilities are required and one is optional.

- **TLS** for the connection.
- **SASL** for authentication, preferring `EXTERNAL` with a client certificate.
- **`account-tag`** so every message carries its sender's account.

Capabilities are negotiated at connect. History additionally requires
`message-tags` and `batch` so replayed messages carry stable `msgid` values. A
server without that complete history capability set still connects, but cannot
restore backlog or reconstruct pre-disconnect conversation turns.

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

Every repository member has a separate certificate and SASL account. When one
IRC server hosts multiple repositories, those accounts remain distinct and are
granted access only to their repository's channels.

Authorization itself stays in the member. Manifest policy and delegate checks
are enforced by the agent, exactly as they are for repository activity. Chat
asks and the agent decides.




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

[`deploy/ergo`](../deploy/ergo) contains a Compose deployment with SASL-only
access, closed registration, persistent SQLite history, and bounded retention.
`radicle-team setup` creates each repository member's separate client
certificate beside its Radicle home and prints the fingerprint used in the
manifest.

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
